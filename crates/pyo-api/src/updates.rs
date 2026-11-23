//! Updates of Pyo from the dashboard.
//!
//! - Checking: a worker loop asks for the latest stable release of
//!   upsetdev/pyo every six hours (or when an admin asks) and keeps its
//!   `release.json` (images pinned by digest, the manifests, notices). It
//!   asks api.pyo.dev, which answers with GitHub's release and counts active
//!   installs, and GitHub itself when that fails or is slow. With "Send
//!   anonymous usage counts" on, the request carries a random install ID
//!   and the version, nothing else. It starts off on a fresh install whose
//!   installer was told `PYO_TELEMETRY=off`. `PYO_UPDATE_CHECK=off` turns checking
//!   off, for installs that can't reach GitHub, and so sends nothing.
//! - Applying (an admin's "Update now", or the maintenance window when
//!   updates are automatic): the agent on Pyo's own cluster runs the new
//!   release's upgrader (agent upgrade.rs); once Pyo is on the new version,
//!   every cluster's agent is moved to the new agent image. Each step is a
//!   row in pyo.updates that the worker moves along, so it carries on across
//!   the very restarts it causes.
//! - A hosted Pyo (Pyo Cloud) is updated by whoever runs it, never from
//!   here. Its agents follow its version ([`hosted_agents`]): each team has
//!   automatic agent updates (the default), or chooses when, and then
//!   updates a cluster's agent from the cluster's page
//!   ([`update_cluster_agent`]). Either way it's the same `UpdateAgent`
//!   command, signed for the cluster like every other.

use std::time::Duration;

use pyo_proto::{Command, Release, UpgradeSpec, parse_version};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    AppState,
    error::{ApiError, ApiResult},
    ids::new_id,
    ops::log_activity,
};

/// Where releases come from.
pub const REPO: &str = "upsetdev/pyo";
/// GitHub's latest release of [`REPO`], through api.pyo.dev, which counts
/// active installs by install ID and version.
const RELEASES_API: &str = "https://api.pyo.dev/v1/releases/latest";
/// After this, GitHub is asked instead.
const RELEASES_API_TIMEOUT: Duration = Duration::from_secs(10);
const CHECK_EVERY_MS: i64 = 6 * 3_600_000;
/// The upgrader has had this long; after that the update is failed.
const PYO_TIMEOUT_MS: i64 = 45 * 60_000;
/// An agent has this long to come back on the new version.
const AGENT_TIMEOUT_MS: i64 = 10 * 60_000;
/// An agent bringing its cluster's install up to Pyo's has this long to
/// finish before it's moved to the new image anyway.
const INSTALL_FIRST_MS: i64 = 3 * 60_000;
/// release.json is small; anything bigger isn't one.
const MAX_ASSET: usize = 4 << 20;
/// Hosts a release check may talk to (GitHub's API, and where it sends
/// release downloads).
const GITHUB_HOSTS: &[&str] = &[
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// This process's version.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn newer(a: &str, b: &str) -> bool {
    matches!((parse_version(a), parse_version(b)), (Some(a), Some(b)) if a > b)
}

/// Said before an update whose agent has other roles than this Pyo's, on
/// an installation whose clusters don't have the self-update role: their
/// agents can't change their own roles, so each cluster's connect command
/// does.
pub const RECONNECT_NOTICE: &str =
    "After updating, run the connect command shown in each cluster's settings.";

/// What to know before updating to `r`: its notices, and [`RECONNECT_NOTICE`]
/// when its agent's roles aren't this Pyo's (a release that doesn't say
/// isn't assumed to change them) and this installation's clusters can't
/// bring them by themselves.
pub fn before_you_update(r: &Release) -> Vec<String> {
    notices(r, crate::install::self_update())
}

/// [`before_you_update`], for an installation whose clusters have the
/// self-update role (`self_update`) or not. With it their agents write the
/// new roles themselves, so nothing is added: a release that needs the
/// command anyway (a new role object, a chart installed with the agent)
/// says so in its own notices.
fn notices(r: &Release, self_update: bool) -> Vec<String> {
    let mut out = r.before_you_update.clone();
    let reconnect = !self_update
        && r.agent_access
            .as_deref()
            .is_some_and(|h| h != pyo_proto::rbac::access_hash());
    if reconnect && !out.iter().any(|n| n == RECONNECT_NOTICE) {
        out.push(RECONNECT_NOTICE.into());
    }
    out
}

// ── Settings ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// "manual" (the default): an admin updates. "auto": in the window.
    pub mode: String,
    /// Days of the maintenance window, "mon".."sun" (UTC).
    pub days: Vec<String>,
    /// Hour of the maintenance window, 0-23 (UTC).
    pub hour: u8,
    /// Look for new releases at all.
    pub check: bool,
    /// Send the install ID with the update check (anonymous usage counts).
    pub usage: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: "manual".into(),
            days: vec!["sun".into()],
            hour: 3,
            check: true,
            usage: true,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> ApiResult<()> {
        if !matches!(self.mode.as_str(), "manual" | "auto") {
            return Err(ApiError::bad_request("Updates are \"manual\" or \"auto\"."));
        }
        if self.hour > 23 {
            return Err(ApiError::bad_request("The hour is 0 to 23 (UTC)."));
        }
        if self.days.iter().any(|d| !DAYS.contains(&d.as_str())) {
            return Err(ApiError::bad_request(
                "Days are mon, tue, wed, thu, fri, sat and sun.",
            ));
        }
        if self.mode == "auto" && self.days.is_empty() {
            return Err(ApiError::bad_request(
                "Pick at least one day for automatic updates.",
            ));
        }
        Ok(())
    }

    /// Whether `now_ms` falls in the maintenance window (that hour, UTC).
    pub fn in_window(&self, now_ms: i64) -> bool {
        let secs = now_ms.div_euclid(1000);
        let days = secs.div_euclid(86_400);
        // 1970-01-01 was a Thursday.
        let weekday = DAYS[((days + 3).rem_euclid(7)) as usize];
        let hour = (secs.rem_euclid(86_400) / 3600) as u8;
        hour == self.hour && self.days.iter().any(|d| d == weekday)
    }
}

pub fn checks_off_by_env() -> bool {
    matches!(
        std::env::var("PYO_UPDATE_CHECK")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "off" | "false" | "0" | "no"
    )
}

/// PYO_TELEMETRY=off, which the installer passes on from a fresh install
/// told so (the pyo-install ConfigMap): usage counts start off.
pub fn telemetry_off_by_env() -> bool {
    off(&std::env::var("PYO_TELEMETRY").unwrap_or_default())
}

fn off(v: &str) -> bool {
    matches!(
        v.to_ascii_lowercase().as_str(),
        "off" | "false" | "0" | "no"
    )
}

/// A fresh install's update settings, saved once with the first admin:
/// usage counts off when the installer was told PYO_TELEMETRY=off, else
/// nothing is saved (the defaults hold). Later they're the admin's.
pub async fn first_default(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> sqlx::Result<()> {
    let Some(s) = first_settings(telemetry_off_by_env()) else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('updates', $1) ON CONFLICT (key) DO NOTHING",
    )
    .bind(serde_json::to_value(&s).unwrap_or_default())
    .execute(&mut **tx)
    .await?;
    tracing::info!("usage counts start off on this install (PYO_TELEMETRY=off)");
    Ok(())
}

fn first_settings(telemetry_off: bool) -> Option<Settings> {
    telemetry_off.then(|| Settings {
        usage: false,
        ..Settings::default()
    })
}

pub async fn settings(db: &PgPool) -> ApiResult<Settings> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'updates'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

pub async fn save_settings(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    s: &Settings,
) -> ApiResult<()> {
    s.validate()?;
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('updates', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
        .bind(serde_json::to_value(s).map_err(anyhow::Error::from)?)
        .execute(&mut **tx)
        .await?;
    // Off drops the ID, so turning it on again starts a new one.
    if !s.usage {
        sqlx::query("DELETE FROM pyo.settings WHERE key = 'install_id'")
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// A random ID for this installation: 16 random bytes, hex. Made once, on
/// the first update check that sends it. It says nothing about the server.
pub fn new_install_id() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

pub fn valid_install_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// This installation's ID, made the first time it's asked for.
async fn install_id(db: &PgPool) -> ApiResult<String> {
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('install_id', $1) ON CONFLICT (key) DO NOTHING")
        .bind(Value::String(new_install_id()))
        .execute(db)
        .await?;
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'install_id'")
            .fetch_optional(db)
            .await?;
    match v {
        Some(Value::String(id)) if valid_install_id(&id) => Ok(id),
        _ => Err(anyhow::anyhow!("the install ID isn't readable").into()),
    }
}

// ── Checking ───────────────────────────────────────────────────────────────

/// The latest release found, with what GitHub says about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub release: Release,
    /// The release's notes (Markdown), as published on GitHub.
    pub notes: String,
    pub url: String,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Checked {
    pub found: Option<Found>,
    pub checked_at: Option<i64>,
    pub error: Option<String>,
}

pub async fn checked(db: &PgPool) -> ApiResult<Checked> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'update_release'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

/// PYO_UPDATE_SOURCE: a mirror with GitHub's release API, e.g. for tests
/// or an air-gapped install. It's then the only place asked.
fn mirror() -> Option<String> {
    std::env::var("PYO_UPDATE_SOURCE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_owned())
}

/// Where GitHub's release API is: GitHub, or the mirror.
fn source() -> String {
    mirror().unwrap_or_else(|| format!("https://api.github.com/repos/{REPO}"))
}

pub(crate) fn client() -> anyhow::Result<reqwest::Client> {
    let mirror = std::env::var("PYO_UPDATE_SOURCE")
        .ok()
        .and_then(|s| reqwest::Url::parse(&s).ok())
        .and_then(|u| u.host_str().map(str::to_owned));
    // Redirects only to GitHub's own hosts over HTTPS (release downloads
    // go to its CDN), or within a configured mirror.
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        let url = attempt.url();
        let host = url.host_str().unwrap_or("").to_owned();
        let github = url.scheme() == "https" && GITHUB_HOSTS.contains(&host.as_str());
        if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else if github || mirror.as_deref() == Some(host.as_str()) {
            attempt.follow()
        } else {
            attempt.error(format!("redirected to {host}, which isn't GitHub"))
        }
    });
    Ok(reqwest::Client::builder()
        .user_agent(format!("pyo/{}", current()))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(policy)
        .build()?)
}

pub(crate) async fn body_limited(
    mut res: reqwest::Response,
    max: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut out = vec![];
    while let Some(chunk) = res.chunk().await? {
        out.extend_from_slice(&chunk);
        if out.len() > max {
            anyhow::bail!("the response is larger than {} MB", max >> 20);
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// One of a release's assets: from GitHub (or the configured mirror) only.
async fn download(http: &reqwest::Client, asset: &GhAsset, tag: &str) -> anyhow::Result<Vec<u8>> {
    if asset.size as usize > MAX_ASSET {
        anyhow::bail!("{tag}'s {} is too large", asset.name);
    }
    let url = reqwest::Url::parse(&asset.browser_download_url)?;
    let from_mirror = std::env::var("PYO_UPDATE_SOURCE")
        .ok()
        .and_then(|s| reqwest::Url::parse(&s).ok())
        .is_some_and(|m| m.host_str() == url.host_str());
    if !(url.scheme() == "https" && GITHUB_HOSTS.contains(&url.host_str().unwrap_or("")))
        && !from_mirror
    {
        anyhow::bail!("{tag}'s {} isn't on GitHub", asset.name);
    }
    let res = http
        .get(url)
        .header("accept", "application/octet-stream")
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("downloading {}: {}", asset.name, res.status());
    }
    body_limited(res, MAX_ASSET).await
}

/// The latest release from api.pyo.dev, with the install ID when usage
/// counts are on, and this version.
async fn from_releases_api(
    http: &reqwest::Client,
    install_id: Option<&str>,
) -> anyhow::Result<GhRelease> {
    let mut req = http
        .get(RELEASES_API)
        .timeout(RELEASES_API_TIMEOUT)
        .header("accept", "application/vnd.github+json")
        .header("pyo-version", current());
    if let Some(id) = install_id {
        req = req.header("pyo-install-id", id);
    }
    let res = req.send().await?;
    if !res.status().is_success() {
        anyhow::bail!("api.pyo.dev answered {}", res.status());
    }
    Ok(serde_json::from_slice(
        &body_limited(res, MAX_ASSET).await?,
    )?)
}

/// GitHub's latest stable release: from api.pyo.dev, or from GitHub when
/// that fails or is slow. With a mirror, from the mirror only.
async fn latest(
    http: &reqwest::Client,
    install_id: Option<&str>,
) -> anyhow::Result<Option<GhRelease>> {
    if mirror().is_none() {
        match from_releases_api(http, install_id).await {
            Ok(gh) => return Ok(Some(gh)),
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "api.pyo.dev didn't answer, asking GitHub")
            }
        }
    }
    let res = http
        .get(format!("{}/releases/latest", source()))
        .header("accept", "application/vnd.github+json")
        .send()
        .await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !res.status().is_success() {
        anyhow::bail!("GitHub answered {}", res.status());
    }
    Ok(Some(serde_json::from_slice(
        &body_limited(res, MAX_ASSET).await?,
    )?))
}

/// The latest stable release, if there is one with a usable release.json.
/// `install_id` goes to api.pyo.dev only (anonymous usage counts).
pub async fn fetch(install_id: Option<&str>) -> anyhow::Result<Option<Found>> {
    let http = client()?;
    let Some(gh) = latest(&http, install_id).await? else {
        return Ok(None);
    };
    if gh.draft || gh.prerelease {
        return Ok(None);
    }
    let Some(asset) = gh.assets.iter().find(|a| a.name == "release.json") else {
        anyhow::bail!(
            "{} has no release.json, so it can't be applied from the dashboard",
            gh.tag_name
        );
    };
    // Signed by Pyo's release workflow (sigstore.rs), or not used at all.
    let Some(signature) = gh
        .assets
        .iter()
        .find(|a| a.name == "release.json.sigstore.json")
    else {
        anyhow::bail!(
            "{} has no signature for its release.json (release.json.sigstore.json), so it can't be trusted",
            gh.tag_name
        );
    };
    let json = download(&http, asset, &gh.tag_name).await?;
    let bundle = download(&http, signature, &gh.tag_name).await?;
    crate::sigstore::verify(
        &json,
        &String::from_utf8_lossy(&bundle),
        &crate::sigstore::RELEASE_SIGNER,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "{}'s release.json isn't signed by Pyo's release workflow: {e:#}",
            gh.tag_name
        )
    })?;
    let release: Release = serde_json::from_slice(&json)
        .map_err(|e| anyhow::anyhow!("{}'s release.json can't be read: {e}", gh.tag_name))?;
    if format!("v{}", release.version) != gh.tag_name && release.version != gh.tag_name {
        anyhow::bail!("{}'s release.json is for {}", gh.tag_name, release.version);
    }
    let problems = release.problems();
    if !problems.is_empty() {
        anyhow::bail!(
            "{}'s release.json can't be used: {}",
            gh.tag_name,
            problems.join(" ")
        );
    }
    Ok(Some(Found {
        release,
        notes: gh.body.unwrap_or_default().chars().take(20_000).collect(),
        url: gh.html_url,
        published_at: gh.published_at,
    }))
}

/// Ask now and remember the answer. A failed check keeps what was found before.
pub async fn check(db: &PgPool) -> ApiResult<Checked> {
    let mut c = checked(db).await?;
    let id = if settings(db).await?.usage {
        Some(install_id(db).await?)
    } else {
        None
    };
    match fetch(id.as_deref()).await {
        Ok(found) => {
            c.found = found;
            c.error = None;
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "update check failed");
            c.error = Some(format!("{e:#}"));
        }
    }
    c.checked_at = Some(pyo_proto::now_ms());
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('update_release', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
        .bind(serde_json::to_value(&c).map_err(anyhow::Error::from)?)
        .execute(db)
        .await?;
    Ok(c)
}

// ── What an update would change ────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct ClusterRow {
    id: String,
    team_id: String,
    name: String,
    team_name: String,
    agent_version: Option<String>,
    updates: i32,
    same_cluster: bool,
    online: bool,
    /// What its agent may do there, as it last reported (`agentScope`).
    scope: Option<Value>,
}

async fn clusters(db: &PgPool) -> ApiResult<Vec<ClusterRow>> {
    Ok(sqlx::query_as(
        "SELECT c.id, c.team_id, c.name, t.name AS team_name, c.agent_version,
                coalesce((c.capabilities->>'updates')::int, 0) AS updates, coalesce(c.same_cluster, false) AS same_cluster,
                (c.last_seen_at > now() - interval '2 minutes') AS online,
                c.capabilities->'agentScope' AS scope
           FROM pyo.clusters c JOIN pyo.teams t ON t.id = c.team_id
          WHERE c.status <> 'pending'
          ORDER BY c.same_cluster DESC, c.name",
    )
    .fetch_all(db)
    .await?)
}

/// Migrations of a release this database hasn't applied.
async fn new_migrations(db: &PgPool, release: &Release) -> ApiResult<Vec<String>> {
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
            .fetch_all(db)
            .await
            .unwrap_or_default();
    Ok(release
        .migrations
        .iter()
        .filter(|m| {
            m.split('_')
                .next()
                .and_then(|n| n.parse::<i64>().ok())
                .is_some_and(|n| !applied.contains(&n))
        })
        .cloned()
        .collect())
}

/// Why `release` can't be started now, if it can't.
fn blockers(release: &Release, clusters: &[ClusterRow], active: bool) -> Vec<String> {
    let mut out = vec![];
    if active {
        out.push("An update is already running.".into());
    }
    if !release.allows_from(current()) {
        out.push(format!(
            "Pyo {} can't update straight to {}: update to {} first. On the server, run: curl -fsSL https://pyo.dev/install.sh | bash -s -- --version {}",
            current(),
            release.version,
            release.min_from.as_deref().unwrap_or("?"),
            release.min_from.as_deref().unwrap_or("?"),
        ));
    }
    if newer(&release.version, current()) {
        match clusters.iter().find(|c| c.same_cluster) {
            None => out.push("Pyo updates itself through an agent on the cluster it runs on, and there's none. Connect this machine as a cluster, or on the server run: curl -fsSL https://pyo.dev/install.sh | bash".into()),
            Some(c) if c.updates < 1 => out.push(format!("The agent on {} is too old to update Pyo. Update once on the server with: curl -fsSL https://pyo.dev/install.sh | bash (later releases can be applied here)", c.name)),
            Some(c) if !c.online => out.push(format!("The agent on {}, the cluster Pyo runs on, isn't reachable.", c.name)),
            Some(_) => {}
        }
    }
    out
}

/// The dashboard's view, for the installation's admin.
pub async fn snapshot(db: &PgPool) -> ApiResult<Value> {
    let s = settings(db).await?;
    let c = checked(db).await?;
    let all = clusters(db).await?;
    let history: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', u.id, 'version', u.version, 'fromVersion', u.from_version, 'trigger', u.trigger, 'status', u.status,
                                  'startedBy', usr.name, 'agents', u.agents, 'message', u.message,
                                  'createdAt', pyo.ms(u.created_at), 'updatedAt', pyo.ms(u.updated_at), 'finishedAt', pyo.ms(u.finished_at))
           FROM pyo.updates u LEFT JOIN pyo.users usr ON usr.id = u.started_by ORDER BY u.created_at DESC LIMIT 5",
    )
    .fetch_all(db)
    .await?;
    let active = history
        .first()
        .filter(|u| matches!(u["status"].as_str(), Some("pyo" | "agents")))
        .cloned();
    let backup: Option<(Option<i64>, bool)> = sqlx::query_as(
        "SELECT pyo.ms(max(finished_at) FILTER (WHERE status = 'succeeded')), EXISTS (SELECT 1 FROM pyo.settings WHERE key = 'backup') FROM pyo.control_backups",
    )
    .fetch_optional(db)
    .await?;
    let (last_backup, backup_configured) = backup.unwrap_or((None, false));

    let mut out = json!({
        "current": current(),
        "settings": s,
        "checksOffByEnv": checks_off_by_env(),
        "checkedAt": c.checked_at,
        "checkError": c.error,
        "latest": null,
        "available": false,
        "plan": null,
        "blockers": [],
        "autoHeld": null,
        "active": active,
        "history": history,
        "backup": { "lastAt": last_backup, "configured": backup_configured || crate::backup::local_store().is_some() },
    });
    let Some(found) = c.found else { return Ok(out) };
    let r = &found.release;
    let to = r.version.as_str();
    let pyo_needed = newer(to, current());
    let agents: Vec<Value> = all
        .iter()
        .map(|c| {
            let from = c.agent_version.clone().unwrap_or_default();
            json!({
                "clusterId": c.id, "name": c.name, "team": c.team_name, "from": from, "to": to,
                "needed": newer(to, &from), "canUpdate": c.updates >= 1, "online": c.online, "sameCluster": c.same_cluster,
            })
        })
        .collect();
    let builders: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', b.id, 'name', b.name, 'kind', b.kind, 'from', coalesce(b.info->>'version', ''), 'team', t.name)
           FROM pyo.builders b JOIN pyo.teams t ON t.id = b.team_id WHERE b.status <> 'removing' ORDER BY b.name",
    )
    .fetch_all(db)
    .await?;
    let builders: Vec<Value> = builders
        .into_iter()
        .map(|mut b| {
            let from = b["from"].as_str().unwrap_or("").to_owned();
            b["to"] = json!(to);
            b["needed"] = json!(from.is_empty() || newer(to, &from));
            // Builders follow Pyo's builder image by themselves.
            b["automatic"] = json!(true);
            b
        })
        .collect();
    let mut platform_images = vec![];
    for o in &r.manifests.platform {
        pyo_proto::collect_images(o, &mut platform_images);
    }
    platform_images.sort();
    platform_images.dedup();
    let migrations = new_migrations(db, r).await?;
    // Databases that run another image on the release restart once.
    let databases_restart = if pyo_needed {
        crate::bundles::restart_count(db, &r.database_images()).await?
    } else {
        0
    };
    let agents_needed = agents.iter().any(|a| {
        a["needed"] == json!(true) && a["canUpdate"] == json!(true) && a["online"] == json!(true)
    });
    let available = pyo_needed || agents_needed;
    let blockers = if available {
        blockers(r, &all, active.is_some())
    } else {
        vec![]
    };
    let auto_held = if !pyo_needed {
        None
    } else if r.manual_only {
        Some(format!(
            "Pyo {to} has to be started by an admin: read what to know first."
        ))
    } else if !r.allows_from(current()) {
        Some(format!(
            "Pyo {to} needs {} or later first.",
            r.min_from.as_deref().unwrap_or("?")
        ))
    } else {
        failed_before(db, to)
            .await?
            .then(|| format!("Updating to {to} failed before, so it isn't retried automatically."))
    };
    out["latest"] = json!({
        "version": to,
        "url": found.url,
        "publishedAt": found.published_at,
        "notes": found.notes,
        "beforeYouUpdate": if pyo_needed { before_you_update(r) } else { r.before_you_update.clone() },
        "manualOnly": r.manual_only,
        "minFrom": r.min_from,
    });
    out["available"] = json!(available);
    out["blockers"] = json!(blockers);
    out["autoHeld"] = json!(auto_held);
    out["plan"] = json!({
        "pyo": { "from": current(), "to": to, "needed": pyo_needed },
        "agents": agents,
        "builders": builders,
        "migrations": migrations,
        "platformImages": platform_images,
        "databasesRestart": databases_restart,
        "images": r.images,
    });
    Ok(out)
}

async fn failed_before(db: &PgPool, version: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.updates WHERE version = $1 AND status = 'failed')",
    )
    .bind(version)
    .fetch_one(db)
    .await?)
}

// ── Starting ───────────────────────────────────────────────────────────────

async fn pyo_team(db: &PgPool) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE same_cluster ORDER BY last_seen_at DESC NULLS LAST LIMIT 1").fetch_optional(db).await?)
}

/// Start updating to `version` (the latest release found). `user` is None
/// for the maintenance window.
pub async fn start(
    s: &AppState,
    version: &str,
    user: Option<&crate::auth::User>,
) -> ApiResult<String> {
    if crate::hosted() {
        return Err(ApiError::bad_request(
            "This Pyo is updated by the people who run it. Its agents follow on their own.",
        ));
    }
    // One start at a time, across processes.
    let mut tx = s.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::bg::LOCK_UPDATES + 1000)
        .execute(&mut *tx)
        .await?;
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.updates WHERE status IN ('pyo', 'agents'))",
    )
    .fetch_one(&mut *tx)
    .await?;
    let c = checked(&s.db).await?;
    let found = c
        .found
        .filter(|f| f.release.version == version)
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "{version} isn't the latest release Pyo knows of. Check for updates and try again."
            ))
        })?;
    let all = clusters(&s.db).await?;
    let r = &found.release;
    let pyo_needed = newer(&r.version, current());
    let agents_needed = all.iter().any(|c| {
        newer(&r.version, c.agent_version.as_deref().unwrap_or("")) && c.updates >= 1 && c.online
    });
    if !pyo_needed && !agents_needed {
        return Err(ApiError::bad_request(format!(
            "Everything is on {version} already."
        )));
    }
    let blocked = blockers(r, &all, active);
    if !blocked.is_empty() {
        return Err(ApiError::bad_request(blocked.join(" ")));
    }
    let id = new_id("upd");
    let status = if pyo_needed { "pyo" } else { "agents" };
    sqlx::query("INSERT INTO pyo.updates (id, version, from_version, trigger, started_by, status, release) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(&id)
        .bind(version)
        .bind(current())
        .bind(if user.is_some() { "manual" } else { "auto" })
        .bind(user.map(|u| u.id.clone()))
        .bind(status)
        .bind(serde_json::to_value(r).map_err(anyhow::Error::from)?)
        .execute(&mut *tx)
        .await?;
    if let Some(team) = pyo_team(&s.db).await? {
        let who = user.map_or("pyo", |u| u.name.as_str());
        let title = if pyo_needed {
            format!("Updating Pyo to {version}")
        } else {
            format!("Updating agents to {version}")
        };
        log_activity(
            &mut tx,
            &team,
            who,
            "pyo.update",
            "info",
            &title,
            json!({ "kind": "update", "id": id, "label": version }),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    tracing::info!(%version, update = %id, "update started");
    Ok(id)
}

// ── Agents on a hosted Pyo ─────────────────────────────────────────────────

/// A team's agent updates: "auto" (the default) or "manual" (it chooses when).
pub const AGENT_UPDATE_MODES: [&str; 2] = ["auto", "manual"];

/// Clusters whose agent may be moved to this Pyo's: online, able to update
/// itself, not on this version, and without an update to this image sent
/// in the last hour (or failed in the last six). `$1`: this version, `$2`:
/// the agent image, `$3`: only teams with automatic updates.
const HOSTED_CANDIDATES: &str = "SELECT c.id, coalesce(c.agent_version, '') FROM pyo.clusters c JOIN pyo.teams t ON t.id = c.team_id
      WHERE c.status <> 'pending' AND c.last_seen_at > now() - interval '2 minutes'
        AND coalesce((c.capabilities->>'updates')::int, 0) >= 1
        AND c.agent_version IS DISTINCT FROM $1
        AND (NOT $3 OR t.agent_updates = 'auto')
        AND NOT EXISTS (SELECT 1 FROM pyo.commands m
                         WHERE m.cluster_id = c.id AND m.command->>'kind' = 'update_agent' AND m.command->>'image' = $2
                           AND m.created_at > now() - CASE WHEN m.status = 'failed' THEN interval '6 hours' ELSE interval '1 hour' END)";

/// Whether `agent` is older than this Pyo (an agent newer, or of a build
/// with no version, is left alone).
fn agent_behind(agent: &str) -> bool {
    newer(current(), agent)
}

/// Agents run only images pinned by digest: `PYO_AGENT_IMAGE` has to be one.
const UNPINNED: &str = "This Pyo's agent image isn't pinned by digest, so agents can't update to it.";

/// Queues the update of one cluster's agent to this Pyo's image.
async fn send_update(s: &AppState, cluster_id: &str) -> ApiResult<String> {
    if !pyo_proto::pinned_image(&s.config.agent_image) {
        return Err(ApiError::bad_request(UNPINNED));
    }
    let mut tx = s.db.begin().await?;
    let id = crate::deploy::enqueue(
        &mut tx,
        cluster_id,
        Command::UpdateAgent {
            image: s.config.agent_image.clone(),
        },
    )
    .await?;
    tx.commit().await?;
    s.wake(cluster_id).await;
    Ok(id)
}

/// A hosted Pyo's automatic agent updates: every minute, each cluster of a
/// team with automatic updates whose agent is behind gets the update.
pub async fn hosted_agents(s: AppState) {
    if !crate::hosted() {
        return;
    }
    if !pyo_proto::pinned_image(&s.config.agent_image) {
        tracing::warn!(image = %s.config.agent_image, "{UNPINNED} Automatic agent updates are off.");
        return;
    }
    loop {
        let st = s.clone();
        let ran = crate::bg::as_leader(&s.db, crate::bg::LOCK_AGENT_UPDATES, || async move {
            let due: Vec<(String, String)> = sqlx::query_as(HOSTED_CANDIDATES)
                .bind(current())
                .bind(&st.config.agent_image)
                .bind(true)
                .fetch_all(&st.db)
                .await?;
            for (cluster, agent) in due.into_iter().filter(|(_, a)| agent_behind(a)) {
                let cmd = send_update(&st, &cluster).await?;
                tracing::info!(%cluster, from = %agent, to = current(), command = %cmd, "updating a cluster's agent");
            }
            ApiResult::Ok(())
        })
        .await;
        if let Some(Err(e)) = ran {
            tracing::warn!(error = %e.message, "hosted agent updates");
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// A cluster's agent, updated now to this Pyo's version: for a hosted Pyo
/// (self-hosted ones update from the Updates page). Fails when it's on it
/// already, isn't reachable, or an update is on its way.
pub async fn update_cluster_agent(s: &AppState, cluster_id: &str) -> ApiResult<String> {
    if !crate::hosted() {
        return Err(ApiError::bad_request("Update agents from Settings › Updates."));
    }
    let row: Option<(Option<String>, bool, i32)> = sqlx::query_as(
        "SELECT agent_version, coalesce(last_seen_at > now() - interval '2 minutes', false), coalesce((capabilities->>'updates')::int, 0)
           FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(&s.db)
    .await?;
    let (agent, online, updates) = row.ok_or_else(|| ApiError::not_found("Cluster"))?;
    let agent = agent.unwrap_or_default();
    if !agent_behind(&agent) {
        return Err(ApiError::bad_request(format!("Its agent is on {} already.", current())));
    }
    if !online {
        return Err(ApiError::bad_request("Its agent isn't reachable. Update it once it's back."));
    }
    if updates < 1 {
        return Err(ApiError::bad_request("Its agent is too old to update itself. Run its connect command again."));
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'update_agent'
                         AND status IN ('pending', 'sent') AND created_at > now() - interval '10 minutes')",
    )
    .bind(cluster_id)
    .fetch_one(&s.db)
    .await?;
    if busy {
        return Err(ApiError::conflict("Its agent is updating."));
    }
    send_update(s, cluster_id).await
}

// ── Moving an update along (worker) ────────────────────────────────────────

pub async fn runner(state: AppState) {
    loop {
        let st = state.clone();
        crate::bg::as_leader(&state.db, crate::bg::LOCK_UPDATES, || async move {
            if let Err(e) = tick(&st).await {
                tracing::warn!(error = %format!("{e:?}"), "update runner");
            }
        })
        .await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn tick(s: &AppState) -> ApiResult<()> {
    // Updated by whoever runs it: its agents follow (hosted_agents).
    if crate::hosted() {
        return Ok(());
    }
    let cfg = settings(&s.db).await?;
    if cfg.check && !checks_off_by_env() {
        let c = checked(&s.db).await?;
        if c.checked_at
            .is_none_or(|t| pyo_proto::now_ms() - t > CHECK_EVERY_MS)
        {
            check(&s.db).await?;
        }
    }
    advance(s).await?;
    if cfg.mode == "auto" && cfg.in_window(pyo_proto::now_ms()) {
        auto_start(s).await?;
    }
    Ok(())
}

async fn auto_start(s: &AppState) -> ApiResult<()> {
    let Some(found) = checked(&s.db).await?.found else {
        return Ok(());
    };
    let r = &found.release;
    let pyo_needed = newer(&r.version, current());
    // Pyo already on it (updated with install.sh): its agents still follow.
    let agents_needed = clusters(&s.db).await?.iter().any(|c| {
        newer(&r.version, c.agent_version.as_deref().unwrap_or("")) && c.updates >= 1 && c.online
    });
    if !(pyo_needed || agents_needed) || failed_before(&s.db, &r.version).await? {
        return Ok(());
    }
    if pyo_needed && (r.manual_only || !r.allows_from(current())) {
        return Ok(());
    }
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.updates WHERE status IN ('pyo', 'agents'))",
    )
    .fetch_one(&s.db)
    .await?;
    // Started already in this window (and it's still going, or done).
    let recent: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.updates WHERE version = $1 AND created_at > now() - interval '2 hours')").bind(&r.version).fetch_one(&s.db).await?;
    if active || recent || !blockers(r, &clusters(&s.db).await?, false).is_empty() {
        return Ok(());
    }
    start(s, &r.version, None).await.map(|_| ())
}

#[derive(Debug, sqlx::FromRow)]
struct UpdateRow {
    id: String,
    version: String,
    status: String,
    release: Value,
    command_id: Option<String>,
    agents: Value,
    trigger: String,
    started_by: Option<String>,
    updated_ms: i64,
}

async fn advance(s: &AppState) -> ApiResult<()> {
    let row: Option<UpdateRow> = sqlx::query_as(
        "SELECT id, version, status, release, command_id, agents, trigger, started_by, pyo.ms(updated_at) AS updated_ms
           FROM pyo.updates WHERE status IN ('pyo', 'agents') ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?;
    let Some(u) = row else { return Ok(()) };
    match u.status.as_str() {
        "pyo" => advance_pyo(s, &u).await,
        _ => advance_agents(s, &u).await,
    }
}

async fn set(s: &AppState, id: &str, status: &str, message: &str) -> ApiResult<()> {
    let done = matches!(status, "succeeded" | "partial" | "failed");
    sqlx::query("UPDATE pyo.updates SET status = $2, message = $3, updated_at = now(), finished_at = CASE WHEN $4 THEN now() ELSE finished_at END WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(message)
        .bind(done)
        .execute(&s.db)
        .await?;
    Ok(())
}

async fn activity(
    s: &AppState,
    team: Option<&str>,
    u: &UpdateRow,
    status: &str,
    title: &str,
    message: &str,
) -> ApiResult<()> {
    let Some(team) = team else { return Ok(()) };
    let who: String = match &u.started_by {
        Some(id) if u.trigger == "manual" => {
            sqlx::query_scalar("SELECT name FROM pyo.users WHERE id = $1")
                .bind(id)
                .fetch_optional(&s.db)
                .await?
                .unwrap_or_else(|| "pyo".into())
        }
        _ => "pyo".into(),
    };
    let mut tx = s.db.begin().await?;
    log_activity(
        &mut tx,
        team,
        &who,
        "pyo.update",
        status,
        title,
        json!({ "kind": "update", "id": u.id, "label": u.version }),
        json!({ "details": { "message": message } }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn advance_pyo(s: &AppState, u: &UpdateRow) -> ApiResult<()> {
    let team = pyo_team(&s.db).await?;
    let release: Release =
        serde_json::from_value(u.release.clone()).map_err(anyhow::Error::from)?;
    let Some(command_id) = &u.command_id else {
        let all = clusters(&s.db).await?;
        let blocked = blockers(&release, &all, false);
        let Some(c) = all
            .iter()
            .find(|c| c.same_cluster)
            .filter(|_| blocked.is_empty())
        else {
            let why = blocked.join(" ");
            set(s, &u.id, "failed", &why).await?;
            return activity(
                s,
                team.as_deref(),
                u,
                "failure",
                &format!("Couldn't update Pyo to {}", u.version),
                &why,
            )
            .await;
        };
        let spec = UpgradeSpec {
            update_id: u.id.clone(),
            image: release.images.agent.clone(),
            release,
        };
        let mut tx = s.db.begin().await?;
        let cmd =
            crate::deploy::enqueue(&mut tx, &c.id, Command::UpgradePyo(Box::new(spec))).await?;
        sqlx::query("UPDATE pyo.updates SET command_id = $2, message = $3, updated_at = now() WHERE id = $1")
            .bind(&u.id)
            .bind(&cmd)
            .bind(format!("Updating Pyo on {}: applying the new release and waiting for it to be ready.", c.name))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        s.wake(&c.id).await;
        return Ok(());
    };
    let cmd: Option<(String, Option<Value>)> =
        sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
            .bind(command_id)
            .fetch_optional(&s.db)
            .await?;
    let (status, result) = cmd.unwrap_or_else(|| {
        (
            "failed".into(),
            Some(json!({ "message": "The update's command is gone." })),
        )
    });
    let message = result
        .as_ref()
        .and_then(|r| r["message"].as_str())
        .unwrap_or("")
        .to_owned();
    match status.as_str() {
        "done" => {
            let msg = format!("Pyo is on {}. Updating the agents.", u.version);
            sqlx::query("UPDATE pyo.updates SET status = 'agents', message = $2, updated_at = now() WHERE id = $1").bind(&u.id).bind(&msg).execute(&s.db).await?;
            activity(
                s,
                team.as_deref(),
                u,
                "success",
                &format!("Updated Pyo to {}", u.version),
                &message,
            )
            .await
        }
        "failed" => {
            let rolled_back = result
                .as_ref()
                .and_then(|r| r["data"]["rolledBack"].as_bool())
                .unwrap_or(false);
            let why = if message.is_empty() {
                "The update failed.".to_owned()
            } else {
                message
            };
            let why = if rolled_back || why.contains("nothing was changed") {
                why
            } else {
                format!("{why} Pyo keeps running the version it was on.")
            };
            set(s, &u.id, "failed", &why).await?;
            activity(
                s,
                team.as_deref(),
                u,
                "failure",
                &format!("Couldn't update Pyo to {}", u.version),
                &why,
            )
            .await
        }
        _ if pyo_proto::now_ms() - u.updated_ms > PYO_TIMEOUT_MS => {
            let why = "No word from the agent for 45 minutes. Pyo keeps running whichever version is ready. Check the pyo-upgrade job in pyo-system.";
            set(s, &u.id, "failed", why).await?;
            activity(
                s,
                team.as_deref(),
                u,
                "failure",
                &format!("Couldn't update Pyo to {}", u.version),
                why,
            )
            .await
        }
        _ => Ok(()),
    }
}

/// Whether an agent waits for its install's refresh (`refreshing`) before
/// it's moved to the new image, `waited_ms` into the agents' step.
fn waits_for_install(refreshing: bool, waited_ms: i64) -> bool {
    refreshing && waited_ms < INSTALL_FIRST_MS
}

async fn advance_agents(s: &AppState, u: &UpdateRow) -> ApiResult<()> {
    let release: Release =
        serde_json::from_value(u.release.clone()).map_err(anyhow::Error::from)?;
    let all = clusters(&s.db).await?;
    let now = pyo_proto::now_ms();
    let mut agents: Vec<Value> = u.agents.as_array().cloned().unwrap_or_default();
    // Entering this step: every cluster whose agent is older.
    if agents.is_empty() {
        agents = all
            .iter()
            .filter(|c| newer(&u.version, c.agent_version.as_deref().unwrap_or("")))
            .map(|c| json!({ "clusterId": c.id, "name": c.name, "from": c.agent_version, "state": "waiting", "at": now }))
            .collect();
        if agents.is_empty() {
            agents.push(json!({ "state": "none" }));
        }
    }
    for a in agents.iter_mut().filter(|a| a["state"] != json!("none")) {
        let id = a["clusterId"].as_str().unwrap_or("").to_owned();
        let Some(c) = all.iter().find(|c| c.id == id) else {
            a["state"] = json!("skipped");
            a["message"] = json!("The cluster was removed.");
            continue;
        };
        let on_new = c.agent_version.as_deref() == Some(u.version.as_str());
        // The admission policy first: the new agent applies what the new
        // policy allows (the gateway's pinned objects), not the old one.
        let refreshing = a["state"] == json!("waiting")
            && !on_new
            && c.online
            && c.updates >= 1
            && match c
                .scope
                .clone()
                .and_then(|v| serde_json::from_value::<pyo_proto::rbac::AgentScope>(v).ok())
            {
                Some(scope) => {
                    crate::install::refreshing(
                        &s.db,
                        &c.id,
                        &scope,
                        c.agent_version.as_deref().unwrap_or(""),
                        c.same_cluster,
                    )
                    .await?
                }
                None => false,
            };
        match a["state"].as_str().unwrap_or("") {
            "waiting" if on_new => a["state"] = json!("done"),
            "waiting" if !c.online => {
                a["state"] = json!("skipped");
                a["message"] = json!("Not reachable. Update it once it's back.");
            }
            "waiting" if c.updates < 1 => {
                a["state"] = json!("skipped");
                a["message"] = json!(format!(
                    "Too old to update itself. On that cluster run: kubectl -n pyo-system set image deployment/pyo-agent agent={}",
                    release.images.agent
                ));
            }
            "waiting" if waits_for_install(refreshing, now - a["at"].as_i64().unwrap_or(now)) => {
                a["message"] = json!("Updating its admission policy first.");
            }
            "waiting" => {
                a.as_object_mut().map(|o| o.remove("message"));
                let mut tx = s.db.begin().await?;
                let cmd = crate::deploy::enqueue(
                    &mut tx,
                    &c.id,
                    Command::UpdateAgent {
                        image: release.images.agent.clone(),
                    },
                )
                .await?;
                tx.commit().await?;
                s.wake(&c.id).await;
                a["state"] = json!("sent");
                a["commandId"] = json!(cmd);
                a["at"] = json!(now);
            }
            "sent" => {
                let cmd: Option<(String, Option<Value>)> =
                    sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
                        .bind(a["commandId"].as_str().unwrap_or(""))
                        .fetch_optional(&s.db)
                        .await?;
                if on_new {
                    a["state"] = json!("done");
                    a.as_object_mut().map(|o| o.remove("message"));
                } else if let Some(("failed", result)) =
                    cmd.as_ref().map(|(st, r)| (st.as_str(), r))
                {
                    a["state"] = json!("failed");
                    a["message"] = json!(
                        result
                            .as_ref()
                            .and_then(|r| r["message"].as_str())
                            .unwrap_or("The agent couldn't update.")
                    );
                } else if now - a["at"].as_i64().unwrap_or(now) > AGENT_TIMEOUT_MS {
                    a["state"] = json!("failed");
                    a["message"] = json!(format!(
                        "Didn't come back on {} within 10 minutes. On that cluster: kubectl -n pyo-system rollout status deployment/pyo-agent (and rollout undo to go back).",
                        u.version
                    ));
                } else if matches!(cmd.as_ref().map(|(st, _)| st.as_str()), Some("done")) {
                    a["message"] = json!("Restarting on the new version.");
                }
            }
            _ => {}
        }
    }
    let settled = agents.iter().all(|a| {
        matches!(
            a["state"].as_str(),
            Some("done" | "failed" | "skipped" | "none")
        )
    });
    let agents_json = json!(agents);
    if !settled {
        if agents_json != u.agents {
            sqlx::query("UPDATE pyo.updates SET agents = $2, updated_at = now() WHERE id = $1")
                .bind(&u.id)
                .bind(&agents_json)
                .execute(&s.db)
                .await?;
        }
        return Ok(());
    }
    sqlx::query("UPDATE pyo.updates SET agents = $2 WHERE id = $1")
        .bind(&u.id)
        .bind(&agents_json)
        .execute(&s.db)
        .await?;
    let failed: Vec<String> = agents
        .iter()
        .filter(|a| matches!(a["state"].as_str(), Some("failed" | "skipped")))
        .map(|a| a["name"].as_str().unwrap_or("?").to_owned())
        .collect();
    let pyo_team = pyo_team(&s.db).await?;
    if failed.is_empty() {
        let msg = format!("Everything is on {}.", u.version);
        set(s, &u.id, "succeeded", &msg).await?;
        // Each team sees its own clusters' agents move.
        for a in agents.iter().filter(|a| a["state"] == json!("done")) {
            if let Some(c) = all
                .iter()
                .find(|c| a["clusterId"].as_str() == Some(c.id.as_str()))
            {
                activity(
                    s,
                    Some(&c.team_id),
                    u,
                    "success",
                    &format!("Updated the agent on {} to {}", c.name, u.version),
                    "",
                )
                .await?;
            }
        }
        if agents.iter().all(|a| a["state"] == json!("none")) {
            activity(
                s,
                pyo_team.as_deref(),
                u,
                "success",
                &format!("Pyo {} is fully up to date", u.version),
                &msg,
            )
            .await?;
        }
        Ok(())
    } else {
        let msg = format!(
            "Pyo is on {}, but not every agent is: {}. See each for why.",
            u.version,
            failed.join(", ")
        );
        set(s, &u.id, "partial", &msg).await?;
        for a in &agents {
            if let Some(c) = all
                .iter()
                .find(|c| a["clusterId"].as_str() == Some(c.id.as_str()))
            {
                let (st, title) = match a["state"].as_str() {
                    Some("done") => (
                        "success",
                        format!("Updated the agent on {} to {}", c.name, u.version),
                    ),
                    _ => (
                        "failure",
                        format!("Couldn't update the agent on {} to {}", c.name, u.version),
                    ),
                };
                activity(
                    s,
                    Some(&c.team_id),
                    u,
                    st,
                    &title,
                    a["message"].as_str().unwrap_or(""),
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_move_to_the_new_image_after_their_install() {
        // Its install being refreshed: the image waits, but not forever.
        assert!(waits_for_install(true, 0));
        assert!(waits_for_install(true, INSTALL_FIRST_MS - 1));
        assert!(!waits_for_install(true, INSTALL_FIRST_MS));
        // Current, or one only its connect command brings up to date: now.
        assert!(!waits_for_install(false, 0));
    }

    /// What every install and update applies to Pyo's own namespace keeps
    /// it closed: nothing in by default, Postgres, ClickHouse and the
    /// registry for Pyo only, the backup store's admin API for Pyo only, the
    /// traffic store for the agent and Vector only, the agent's relay for
    /// Vector only, and no plain registry on the tailnet.
    #[test]
    fn pyos_namespace_is_closed() {
        let docs: Vec<Value> = [
            include_str!("../../../deploy/k8s/platform.yaml"),
            include_str!("../../../deploy/k8s/pyo.yaml"),
        ]
        .join("\n---\n")
        .split("\n---")
        .filter_map(|d| serde_norway::from_str::<Value>(d).ok())
        .filter(|d| d.is_object())
        .collect();
        let find = |kind: &str, name: &str| {
            docs.iter()
                .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
                .unwrap_or_else(|| panic!("{kind} {name}"))
                .clone()
        };
        let deny = find("NetworkPolicy", "default-deny");
        assert_eq!(deny["spec"]["podSelector"], json!({}));
        assert!(deny["spec"]["ingress"].is_null(), "nothing in");
        let only_pyo = |name: &str, port: i64| {
            let p = find("NetworkPolicy", name);
            let rules = p["spec"]["ingress"].as_array().unwrap();
            let rule = rules
                .iter()
                .find(|r| {
                    r["ports"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|x| x["port"] == port)
                })
                .unwrap_or_else(|| panic!("{name}:{port}"));
            assert_eq!(
                rule["from"][0]["podSelector"]["matchExpressions"][0]["values"],
                json!(["pyo", "pyo-worker"]),
                "{name}:{port} answers Pyo only"
            );
        };
        only_pyo("pyo-databases", 5432);
        only_pyo("pyo-databases", 8123);
        only_pyo("registry", 5000);
        only_pyo("pyo-backup-store", 3903);
        // The traffic store: the agent and Vector only, on its port. By
        // their pods' labels, so from any server: the store, the agent
        // and each server's Vector are often on different servers.
        let agent_label = crate::install::render("http://pyo.pyo-system.svc:8000", "agent:1", "t")
            .split("\n---")
            .filter_map(|d| serde_norway::from_str::<Value>(d).ok())
            .find(|d| d["kind"] == "Deployment" && d["metadata"]["name"] == "pyo-agent")
            .expect("the agent's Deployment")["spec"]["template"]["metadata"]["labels"]
            ["app.kubernetes.io/name"]
            .clone();
        assert_eq!(agent_label, "pyo-agent");
        let store = find("NetworkPolicy", pyo_proto::traffic::STORE);
        assert_eq!(
            store["spec"]["podSelector"],
            json!({ "matchLabels": { "app.kubernetes.io/name": pyo_proto::traffic::STORE } })
        );
        assert_eq!(store["spec"]["policyTypes"], json!(["Ingress"]));
        assert_eq!(
            store["spec"]["ingress"],
            json!([{
                "from": [{ "podSelector": { "matchExpressions": [{
                    "key": "app.kubernetes.io/name", "operator": "In",
                    "values": [agent_label, pyo_proto::traffic::VECTOR],
                }] } }],
                "ports": [{ "port": pyo_proto::traffic::STORE_PORT }],
            }]),
            "the traffic store answers the agent and Vector only"
        );
        // The agent: its relay, for Vector only, from any server.
        let agent = find("NetworkPolicy", "pyo-agent");
        assert_eq!(
            agent["spec"]["podSelector"],
            json!({ "matchLabels": { "app.kubernetes.io/name": agent_label } })
        );
        assert_eq!(agent["spec"]["policyTypes"], json!(["Ingress"]));
        assert_eq!(
            agent["spec"]["ingress"],
            json!([{
                "from": [{ "podSelector": { "matchLabels": { "app.kubernetes.io/name": pyo_proto::traffic::VECTOR } } }],
                "ports": [{ "port": pyo_proto::system::AGENT_RELAY_PORT }],
            }]),
            "the agent's relay answers Vector only"
        );
        // Pyo's own front: never from a project's pods (Pyo believes
        // X-Forwarded-For from the pod network). Its own namespace, the
        // gateway's, pyo-static's, import Jobs outside projects, and the
        // servers' own ends of the pod network only.
        let front = find("NetworkPolicy", "pyo");
        let rules = front["spec"]["ingress"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        for r in rules {
            assert_eq!(r["ports"], json!([{ "port": 8000 }, { "port": 5006 }]));
            assert!(
                !r["from"].as_array().unwrap().is_empty(),
                "never from anywhere"
            );
        }
        assert_eq!(
            rules[0]["from"],
            json!([
                { "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "pyo-system" } } },
                { "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": pyo_proto::system::GATEWAY_NS } } },
                { "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": pyo_proto::system::STATIC_NS } } },
                {
                    "namespaceSelector": { "matchExpressions": [{ "key": pyo_proto::rbac::PROJECT_LABEL, "operator": "DoesNotExist" }] },
                    "podSelector": { "matchLabels": { pyo_proto::dbimport::POD_LABEL.0: pyo_proto::dbimport::POD_LABEL.1 } },
                },
            ])
        );
        let servers: Vec<String> = rules[1]["from"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["ipBlock"]["cidr"].as_str().unwrap().to_owned())
            .collect();
        let mut every: Vec<String> = (0..256).map(|n| format!("10.42.{n}.0/24")).collect();
        every.sort();
        let mut want = pyo_proto::system::tailnet_server_cidrs(every.iter().map(String::as_str));
        let mut have = servers.clone();
        want.sort();
        have.sort();
        assert_eq!(have, want, "each server's first /31, which pods never get");
        // The registry isn't on the host's network; the tailnet gets only the door.
        let registry = find("Deployment", "registry");
        assert!(registry["spec"]["template"]["spec"]["hostNetwork"].is_null());
        let host = find("Deployment", "pyo-host");
        assert_eq!(
            host["spec"]["template"]["spec"]["containers"][0]["args"],
            json!([
                "TCP-LISTEN:5005,bind=127.0.0.1,fork,reuseaddr",
                "TCP:pyo-registry.pyo-system.svc.cluster.local:5000"
            ]),
            "the plain registry on loopback only"
        );
        let forward = find("Deployment", "pyo-forward");
        assert_eq!(
            forward["spec"]["template"]["spec"]["containers"][0]["args"],
            json!([
                "forward",
                "8000/local,private,tailnet=pyo.pyo-system.svc.cluster.local:8000",
                "5006/tailnet=pyo-registry-door.pyo-system.svc.cluster.local:5006",
            ]),
            "the tailnet gets only the door"
        );
        // The backup store (Garage): RPC on localhost, a pinned image, not root.
        let config = find("ConfigMap", "pyo-backup-store")["data"]["garage.toml"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            config.contains("rpc_bind_addr = \"127.0.0.1:3901\"")
                && config.contains("s3_region = \"garage\"")
                && config.contains("replication_factor = 1")
        );
        assert!(
            !config.contains("rpc_secret") && !config.contains("admin_token"),
            "secrets come from the secret"
        );
        let store = find("StatefulSet", "pyo-backup-store");
        let pod = &store["spec"]["template"]["spec"];
        assert!(
            pod["containers"][0]["image"]
                .as_str()
                .unwrap()
                .starts_with("dxflrs/garage@sha256:")
        );
        assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
        assert_eq!(
            pod["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
            true
        );
        let s3 = find("NetworkPolicy", "pyo-backup-store")["spec"]["ingress"][0].clone();
        assert!(
            s3["from"].is_null() && s3["ports"] == json!([{ "port": 3900 }]),
            "only S3 from anywhere"
        );
        // ClickHouse runs small: its settings file and Pyo's profile are
        // mounted one file each (the image writes Pyo's user into users.d),
        // and what it may allocate is a share of the pod's limit, which it
        // reads as the machine's memory.
        let ch_config = find("ConfigMap", "pyo-clickhouse")["data"].clone();
        let server = ch_config["config.xml"].as_str().unwrap();
        assert!(server.contains("<query_log remove=\"1\"/>"));
        assert!(
            server.contains("<max_server_memory_usage>0</max_server_memory_usage>")
                && server.contains("<max_server_memory_usage_to_ram_ratio>0.7<")
        );
        assert!(
            ch_config["users.xml"]
                .as_str()
                .unwrap()
                .contains("<max_memory_usage>")
        );
        let ch = find("StatefulSet", "pyo-clickhouse");
        let ch_pod = &ch["spec"]["template"]["spec"];
        let mounts = ch_pod["containers"][0]["volumeMounts"].to_string();
        assert!(
            mounts.contains("/etc/clickhouse-server/config.d/pyo.xml")
                && mounts.contains("/etc/clickhouse-server/users.d/pyo.xml")
        );
        assert_eq!(ch_pod["volumes"][0]["configMap"]["name"], "pyo-clickhouse");
        assert!(
            ch_pod["containers"][0]["resources"]["limits"]["memory"].is_string(),
            "the ratio needs a limit"
        );
        // Every object is one the upgrader applies (agent upgrade.rs KINDS).
        for d in &docs {
            let k = d["kind"].as_str().unwrap();
            assert!(
                [
                    "Namespace",
                    "NetworkPolicy",
                    "PersistentVolumeClaim",
                    "Service",
                    "Deployment",
                    "StatefulSet",
                    "ConfigMap",
                    "PodDisruptionBudget"
                ]
                .contains(&k),
                "{k}"
            );
        }
    }

    #[test]
    fn windows() {
        let s = Settings {
            mode: "auto".into(),
            days: vec!["sun".into(), "wed".into()],
            hour: 3,
            check: true,
            usage: true,
        };
        // 2026-11-01 was a Sunday.
        let sunday_3am = 1_793_502_000_000; // 2026-11-01T03:00:00Z
        assert!(s.in_window(sunday_3am));
        assert!(s.in_window(sunday_3am + 59 * 60_000));
        assert!(!s.in_window(sunday_3am + 60 * 60_000));
        assert!(!s.in_window(sunday_3am + 86_400_000)); // Monday
        assert!(s.in_window(sunday_3am + 3 * 86_400_000)); // Wednesday
    }

    #[test]
    fn settings_are_checked() {
        assert!(Settings::default().validate().is_ok());
        assert_eq!(Settings::default().mode, "manual");
        assert!(
            Settings {
                mode: "sometimes".into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                hour: 24,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                days: vec!["funday".into()],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                mode: "auto".into(),
                days: vec![],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        // Old or partial settings read with defaults.
        let s: Settings = serde_json::from_value(json!({ "mode": "auto" })).unwrap();
        assert_eq!(s.hour, 3);
        assert!(s.usage);
    }

    #[test]
    fn telemetry_off_starts_usage_counts_off() {
        let s = first_settings(true).unwrap();
        assert!(!s.usage);
        // Everything else as a new install has it, update checks included.
        assert_eq!(Settings { usage: true, ..s }, Settings::default());
        assert_eq!(first_settings(false), None);
        for v in ["off", "OFF", "false", "0", "no"] {
            assert!(off(v), "{v}");
        }
        for v in ["", "on", "1", "yes"] {
            assert!(!off(v), "{v}");
        }
    }

    #[test]
    fn install_ids_are_random_hex() {
        let a = new_install_id();
        assert!(valid_install_id(&a));
        assert_ne!(a, new_install_id());
        assert!(!valid_install_id(&a.to_uppercase()));
        assert!(!valid_install_id("pyo.example.com"));
        assert!(!valid_install_id(&a[1..]));
    }

    #[test]
    fn versions_compare() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.2.0", ""));
        assert!(!newer("garbage", "0.1.0"));
    }

    fn cluster(same: bool, updates: i32, online: bool) -> ClusterRow {
        ClusterRow {
            id: "cls_1".into(),
            team_id: "t".into(),
            name: "main".into(),
            team_name: "T".into(),
            agent_version: Some("0.1.0".into()),
            updates,
            same_cluster: same,
            online,
            scope: None,
        }
    }

    /// An update whose agent has other roles says, before it starts, that
    /// each cluster needs its connect command; one with the same roles, or
    /// a release that doesn't say, doesn't. Nor does an installation whose
    /// agents bring their roles themselves.
    #[test]
    fn other_agent_roles_ask_for_the_connect_command() {
        let before_you_update = |r: &Release| notices(r, false);
        let with = |access: Option<String>, notices: &[&str]| Release {
            version: "99.0.0".into(),
            agent_access: access,
            before_you_update: notices.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        };
        let ours = pyo_proto::rbac::access_hash();
        assert!(before_you_update(&with(Some(ours.clone()), &[])).is_empty());
        assert!(before_you_update(&with(None, &[])).is_empty());
        assert_eq!(
            before_you_update(&with(
                Some("0000000000000000".into()),
                &["Postgres restarts."]
            )),
            ["Postgres restarts.", RECONNECT_NOTICE]
        );
        // Written by hand as well: once.
        assert_eq!(
            before_you_update(&with(Some("0000000000000000".into()), &[RECONNECT_NOTICE])),
            [RECONNECT_NOTICE]
        );
        // With the self-update role the agents write the new roles: only
        // the release's own notices.
        assert_eq!(
            notices(&with(Some("0000000000000000".into()), &["Postgres restarts."]), true),
            ["Postgres restarts."]
        );
        assert_eq!(
            notices(&with(Some("0000000000000000".into()), &[RECONNECT_NOTICE]), true),
            [RECONNECT_NOTICE]
        );
    }

    #[test]
    fn what_stops_an_update() {
        let r = Release {
            version: "99.0.0".into(),
            ..Default::default()
        };
        assert!(blockers(&r, &[cluster(true, 1, true)], false).is_empty());
        assert!(blockers(&r, &[cluster(true, 1, true)], true)[0].contains("already running"));
        assert!(blockers(&r, &[cluster(false, 1, true)], false)[0].contains("there's none"));
        assert!(blockers(&r, &[cluster(true, 0, true)], false)[0].contains("too old"));
        assert!(blockers(&r, &[cluster(true, 1, false)], false)[0].contains("isn't reachable"));
        let r = Release {
            version: "99.0.0".into(),
            min_from: Some("98.0.0".into()),
            ..Default::default()
        };
        assert!(
            blockers(&r, &[cluster(true, 1, true)], false)[0].contains("update to 98.0.0 first")
        );
    }
}

#[cfg(test)]
mod hosted_tests {
    use super::*;

    #[test]
    fn only_older_agents_are_behind() {
        let v = current();
        assert!(!agent_behind(v));
        assert!(agent_behind("0.0.1"));
        assert!(!agent_behind("99.0.0"));
        // A build with no version is left alone.
        assert!(!agent_behind(""));
    }

    /// Against a throwaway Postgres: the clusters due an update are those
    /// online, able to update, behind, of teams with automatic updates (or
    /// any team, asked for by hand), without an update to this image sent in
    /// the last hour or failed in the last six.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn agents_of_teams_with_automatic_updates_are_due() {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        let run = crate::ids::random_string(8);
        let (auto, manual) = (format!("team_u{run}"), format!("team_m{run}"));
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&auto).execute(&db).await.unwrap();
        q("INSERT INTO pyo.teams (id, name, slug, agent_updates) VALUES ($1, $1, $1, 'manual')").bind(&manual).execute(&db).await.unwrap();
        let image = format!("agent:{run}");
        let add = |id: &str, team: &str, version: &str, online: bool, updates: i32| {
            let (db, id, team, version) = (db.clone(), format!("{id}{run}"), team.to_owned(), version.to_owned());
            async move {
                sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, agent_version, last_seen_at, capabilities)
                             VALUES ($1, $2, $1, $1, 'connected', $3, now() - CASE WHEN $4 THEN interval '0' ELSE interval '1 hour' END, $5)")
                    .bind(&id).bind(&team).bind(&version).bind(online).bind(json!({ "updates": updates })).execute(&db).await.unwrap();
                id
            }
        };
        let behind = add("cl_b", &auto, "0.0.1", true, 1).await;
        let theirs = add("cl_m", &manual, "0.0.1", true, 1).await;
        let current_one = add("cl_c", &auto, current(), true, 1).await;
        let offline = add("cl_o", &auto, "0.0.1", false, 1).await;
        let too_old = add("cl_t", &auto, "0.0.1", true, 0).await;
        let sent = add("cl_s", &auto, "0.0.1", true, 1).await;
        let failed_long_ago = add("cl_f", &auto, "0.0.1", true, 1).await;
        let failed_now = add("cl_n", &auto, "0.0.1", true, 1).await;
        let cmd = |id: &str, cluster: &str, status: &str, age: &str| {
            let (db, id, cluster, status, age, image) = (db.clone(), format!("{id}{run}"), cluster.to_owned(), status.to_owned(), age.to_owned(), image.clone());
            async move {
                sqlx::query("INSERT INTO pyo.commands (id, cluster_id, command, status, created_at) VALUES ($1, $2, $3, $4, now() - $5::interval)")
                    .bind(&id).bind(&cluster).bind(json!({ "kind": "update_agent", "image": image })).bind(&status).bind(&age).execute(&db).await.unwrap();
            }
        };
        cmd("cmd_s", &sent, "done", "10 minutes").await;
        cmd("cmd_f", &failed_long_ago, "failed", "7 hours").await;
        cmd("cmd_n", &failed_now, "failed", "2 hours").await;
        let due = |only_auto: bool| {
            let (db, image, run) = (db.clone(), image.clone(), run.clone());
            async move {
                let rows: Vec<(String, String)> = sqlx::query_as(HOSTED_CANDIDATES).bind(current()).bind(&image).bind(only_auto).fetch_all(&db).await.unwrap();
                let mut ids: Vec<String> = rows.into_iter().filter(|(id, a)| id.ends_with(&run) && agent_behind(a)).map(|r| r.0).collect();
                ids.sort();
                ids
            }
        };
        let mut want = vec![behind.clone(), failed_long_ago.clone()];
        want.sort();
        assert_eq!(due(true).await, want);
        assert!(due(false).await.contains(&theirs));
        for not in [&current_one, &offline, &too_old, &sent, &failed_now] {
            assert!(!due(false).await.contains(not), "{not} isn't due");
        }
    }
}
