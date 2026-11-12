//! GitHub, through a GitHub App that belongs to this Pyo installation.
//!
//! - An admin creates the app from Settings with GitHub's manifest flow:
//!   Pyo submits a manifest, GitHub creates the app and hands back its id
//!   and keys, which are sealed and kept in `pyo.settings` ('github').
//! - A team installs the app on a GitHub account or organization (choosing
//!   which repositories it may see); the installation is linked to the team.
//! - Deploys of those repositories clone with a short-lived installation
//!   token, so private repositories work.
//! - A push deploys the app: GitHub's webhook when Pyo is reachable from the
//!   internet, otherwise a poll of each branch head every minute.
//! - Each deploy reports a commit status on GitHub (pending, success,
//!   failure) linking to the deployment.

use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect},
    routing::{get, post},
};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Mutex;

use crate::{
    AppState,
    auth::SessionUser,
    deploy::{self, DeployRequest},
    error::{ApiError, ApiResult},
    ids::{new_id, secret_token},
    ops::{Role, log_activity, require_role},
};

const API: &str = "https://api.github.com";

/// The app, as saved after the manifest flow. Secrets are sealed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GithubApp {
    pub app_id: i64,
    pub slug: String,
    pub name: String,
    pub html_url: String,
    pub owner: String,
    pub client_id: String,
    pub pem_sealed: String,
    pub webhook_secret_sealed: String,
    pub client_secret_sealed: String,
    /// Where Pyo is reached; webhooks go to {public_url}/api/github/webhook.
    pub public_url: String,
    /// Whether GitHub sends webhooks (only with a public https URL).
    pub webhook_active: bool,
}

impl GithubApp {
    /// For the dashboard: no keys.
    pub fn public(&self) -> Value {
        json!({ "slug": self.slug, "name": self.name, "htmlUrl": self.html_url, "owner": self.owner, "publicUrl": self.public_url, "webhookActive": self.webhook_active })
    }
}

pub async fn app(db: &sqlx::PgPool) -> ApiResult<Option<GithubApp>> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'github'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok())
        .filter(|a: &GithubApp| a.app_id > 0))
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("pyo")
            .timeout(Duration::from_secs(20))
            .build()
            .expect("http client")
    })
}

/// One-time values for redirects through GitHub: manifest creation (admin)
/// and app installation (a team). Kept in memory for 30 minutes.
#[derive(Clone)]
enum Pending {
    Manifest {
        user_id: String,
        public_url: String,
        webhook: bool,
    },
    Install {
        user_id: String,
        team_id: String,
    },
    /// Installed; waiting for GitHub to tell who the person is, to check
    /// they can see the installation.
    Authorize {
        user_id: String,
        team_id: String,
        installation_id: i64,
    },
}

fn pending() -> &'static Mutex<HashMap<String, (Instant, Pending)>> {
    static P: OnceLock<Mutex<HashMap<String, (Instant, Pending)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn remember(p: Pending) -> String {
    let state = secret_token();
    let mut g = pending().lock().await;
    g.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(1800));
    g.insert(state.clone(), (Instant::now(), p));
    state
}

async fn take(state: &str) -> Option<Pending> {
    pending()
        .lock()
        .await
        .remove(state)
        .filter(|(t, _)| t.elapsed() < Duration::from_secs(1800))
        .map(|(_, p)| p)
}

/// A public https URL that isn't a private address can receive webhooks.
fn reachable(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', ':']).next().unwrap_or("");
    !(host == "localhost"
        || host.ends_with(".local")
        || host.ends_with(".localhost")
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("100."))
}

// ── Manifest flow (admin) ────────────────────────────────────────────────────

/// What the dashboard posts to GitHub to create the app.
pub async fn manifest(user_id: &str, public_url: &str, org: Option<&str>) -> ApiResult<Value> {
    let base = public_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(ApiError::bad_request(
            "The dashboard URL starts with http:// or https://.",
        ));
    }
    let webhook = reachable(base);
    let suffix: String = secret_token()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>()
        .to_lowercase();
    let manifest = json!({
        "name": format!("Pyo {suffix}"),
        "url": base,
        "description": "Deploys this account's repositories with Pyo.",
        "public": false,
        "redirect_url": format!("{base}/api/github/created"),
        "setup_url": format!("{base}/api/github/installed"),
        "setup_on_update": true,
        "callback_urls": [format!("{base}/api/github/authorized")],
        "hook_attributes": { "url": format!("{base}/api/github/webhook"), "active": webhook },
        "default_events": ["push"],
        "default_permissions": { "contents": "read", "metadata": "read", "statuses": "write" },
    });
    let state = remember(Pending::Manifest {
        user_id: user_id.to_owned(),
        public_url: base.to_owned(),
        webhook,
    })
    .await;
    let action = match org.map(str::trim).filter(|o| !o.is_empty()) {
        Some(o) => format!("https://github.com/organizations/{o}/settings/apps/new?state={state}"),
        None => format!("https://github.com/settings/apps/new?state={state}"),
    };
    Ok(
        json!({ "ok": true, "action": action, "manifest": manifest.to_string(), "webhook": webhook }),
    )
}

#[derive(Deserialize)]
struct CreatedQuery {
    code: String,
    state: String,
}

/// GitHub sends the admin back here with a code for the new app's keys.
async fn created(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<CreatedQuery>,
) -> ApiResult<Redirect> {
    let Some(Pending::Manifest {
        user_id,
        public_url,
        webhook,
    }) = take(&q.state).await
    else {
        return Ok(Redirect::to("/settings?github=expired"));
    };
    if user_id != user.id || !user.is_admin {
        return Err(ApiError::forbidden(
            "Only the admin who started this can finish it.",
        ));
    }
    let res = http()
        .post(format!("{API}/app-manifests/{}/conversions", q.code))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(anyhow::Error::from)?;
    if !res.status().is_success() {
        tracing::warn!(status = %res.status(), "GitHub app conversion failed");
        return Ok(Redirect::to("/settings?github=failed"));
    }
    let v: Value = res.json().await.map_err(anyhow::Error::from)?;
    let seal = |k: &str| s.sealer.seal(v[k].as_str().unwrap_or(""));
    let app = GithubApp {
        app_id: v["id"].as_i64().unwrap_or(0),
        slug: v["slug"].as_str().unwrap_or("").to_owned(),
        name: v["name"].as_str().unwrap_or("").to_owned(),
        html_url: v["html_url"].as_str().unwrap_or("").to_owned(),
        owner: v["owner"]["login"].as_str().unwrap_or("").to_owned(),
        client_id: v["client_id"].as_str().unwrap_or("").to_owned(),
        pem_sealed: seal("pem")?,
        webhook_secret_sealed: seal("webhook_secret")?,
        client_secret_sealed: seal("client_secret")?,
        public_url,
        webhook_active: webhook,
    };
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('github', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()")
        .bind(serde_json::to_value(&app).map_err(anyhow::Error::from)?)
        .execute(&s.db)
        .await?;
    Ok(Redirect::to("/settings?github=created"))
}

// ── Installations (teams) ────────────────────────────────────────────────────

/// The GitHub page where a team installs the app.
pub async fn install_url(s: &AppState, user_id: &str, team_id: &str) -> ApiResult<String> {
    let app = app(&s.db).await?.ok_or_else(|| {
        ApiError::bad_request(
            "GitHub isn't set up on this installation yet. An admin can set it up in Settings.",
        )
    })?;
    let state = remember(Pending::Install {
        user_id: user_id.to_owned(),
        team_id: team_id.to_owned(),
    })
    .await;
    Ok(format!(
        "https://github.com/apps/{}/installations/new?state={state}",
        app.slug
    ))
}

#[derive(Deserialize)]
struct InstalledQuery {
    installation_id: Option<i64>,
    state: Option<String>,
}

/// GitHub sends the person back here after installing (or changing) the app.
///
/// The `installation_id` GitHub puts in the URL can be edited by whoever
/// follows it, so it isn't trusted: the person signs in with GitHub next
/// (`authorized`), and the installation is linked only when it's one of
/// theirs. An installation linked to another team stays there.
async fn installed(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<InstalledQuery>,
) -> ApiResult<Redirect> {
    let Some(id) = q.installation_id else {
        return Ok(Redirect::to("/"));
    };
    let pending = match q.state.as_deref() {
        Some(st) => take(st).await,
        None => None,
    };
    let team_id = match pending {
        Some(Pending::Install { user_id, team_id }) if user_id == user.id => team_id,
        _ => {
            // Changes made on GitHub later come back without a state: the
            // link stays as it is.
            let linked: Option<String> =
                sqlx::query_scalar("SELECT team_id FROM pyo.github_installations WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&s.db)
                    .await?;
            let Some(t) = linked else {
                return Ok(Redirect::to("/?github=expired"));
            };
            let mut tx = s.db.begin().await?;
            let member = require_role(&mut tx, &user, &t, Role::Viewer).await.is_ok();
            drop(tx);
            return Ok(Redirect::to(&if member {
                format!("/{}/integrations", team_slug(&s, &t).await?)
            } else {
                "/".to_owned()
            }));
        }
    };
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &team_id, Role::Admin).await?;
    tx.commit().await?;
    let app = app(&s.db)
        .await?
        .ok_or_else(|| ApiError::bad_request("GitHub isn't set up on this installation."))?;
    let state = remember(Pending::Authorize {
        user_id: user.id.clone(),
        team_id,
        installation_id: id,
    })
    .await;
    Ok(Redirect::to(&authorize_url(&app, &state)))
}

/// GitHub's sign-in page for the app, coming back to `authorized`.
fn authorize_url(app: &GithubApp, state: &str) -> String {
    let redirect = format!(
        "{}/api/github/authorized",
        app.public_url.trim_end_matches('/')
    );
    reqwest::Url::parse_with_params(
        "https://github.com/login/oauth/authorize",
        [
            ("client_id", app.client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("state", state),
        ],
    )
    .map(String::from)
    .unwrap_or_default()
}

#[derive(Deserialize)]
struct AuthorizedQuery {
    code: Option<String>,
    state: Option<String>,
}

/// GitHub sends the person back here after signing in with the app: link
/// the installation when their GitHub account can see it.
async fn authorized(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<AuthorizedQuery>,
) -> ApiResult<Redirect> {
    let pending = match q.state.as_deref() {
        Some(st) => take(st).await,
        None => None,
    };
    let Some(Pending::Authorize {
        user_id,
        team_id,
        installation_id: id,
    }) = pending
    else {
        return Ok(Redirect::to("/?github=expired"));
    };
    if user_id != user.id {
        return Ok(Redirect::to("/?github=expired"));
    }
    let slug = team_slug(&s, &team_id).await?;
    let back = |result: &str| Redirect::to(&format!("/{slug}/integrations?github={result}"));
    let Some(code) = q.code.filter(|c| !c.is_empty()) else {
        return Ok(back("denied"));
    };
    let app = app(&s.db)
        .await?
        .ok_or_else(|| ApiError::bad_request("GitHub isn't set up on this installation."))?;
    let visible = match user_installations(&s, &app, &code).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "GitHub sign-in failed");
            return Ok(back("failed"));
        }
    };
    if !visible.contains(&id) {
        return Ok(back("denied"));
    }
    // Ask GitHub about it with the app's own key, for the account's name.
    let inst = app_get(&s, &format!("/app/installations/{id}"))
        .await
        .map_err(|e| {
            ApiError::bad_request(format!("GitHub doesn't know that installation: {e:#}"))
        })?;
    let account = inst["account"]["login"].as_str().unwrap_or("").to_owned();
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &team_id, Role::Admin).await?;
    if !link_installation(
        &mut tx,
        id,
        &team_id,
        &account,
        inst["account"]["type"].as_str().unwrap_or("User"),
        inst["repository_selection"].as_str().unwrap_or("selected"),
    )
    .await?
    {
        return Ok(back("taken"));
    }
    log_activity(
        &mut tx,
        &team_id,
        &user.name,
        "integration.saved",
        "success",
        &format!("Connected GitHub ({account})"),
        json!({ "kind": "team", "id": team_id, "label": account }),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(back("connected"))
}

async fn team_slug(s: &AppState, team_id: &str) -> ApiResult<String> {
    Ok(
        sqlx::query_scalar("SELECT slug FROM pyo.teams WHERE id = $1")
            .bind(team_id)
            .fetch_one(&s.db)
            .await?,
    )
}

/// Link an installation to a team, or refresh it when it's already the
/// team's. False when another team has it: that team disconnects it first.
pub(crate) async fn link_installation(
    tx: &mut sqlx::PgConnection,
    id: i64,
    team_id: &str,
    account: &str,
    account_type: &str,
    selection: &str,
) -> ApiResult<bool> {
    let linked: Option<i64> = sqlx::query_scalar(
        "INSERT INTO pyo.github_installations (id, team_id, account, account_type, repository_selection) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id) DO UPDATE SET account = excluded.account, account_type = excluded.account_type, repository_selection = excluded.repository_selection
         WHERE pyo.github_installations.team_id = excluded.team_id
         RETURNING id",
    )
    .bind(id)
    .bind(team_id)
    .bind(account)
    .bind(account_type)
    .bind(selection)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(linked.is_some())
}

/// The installations of this app that the GitHub user who signed in (with
/// `code`) can see.
async fn user_installations(s: &AppState, app: &GithubApp, code: &str) -> anyhow::Result<Vec<i64>> {
    let secret = s.sealer.open(&app.client_secret_sealed)?;
    let redirect = format!(
        "{}/api/github/authorized",
        app.public_url.trim_end_matches('/')
    );
    let res = http()
        .post("https://github.com/login/oauth/access_token")
        .header("accept", "application/json")
        .json(&json!({ "client_id": app.client_id, "client_secret": secret, "code": code, "redirect_uri": redirect }))
        .send()
        .await?;
    let v: Value = res.json().await?;
    let Some(token) = v["access_token"].as_str().filter(|t| !t.is_empty()) else {
        anyhow::bail!(
            "GitHub gave no token ({})",
            v["error"].as_str().unwrap_or("unknown error")
        );
    };
    let mut ids = vec![];
    for page in 1..=10 {
        let res = http()
            .get(format!("{API}/user/installations?per_page=100&page={page}"))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .send()
            .await?;
        if !res.status().is_success() {
            anyhow::bail!("GitHub answered {} to the installation list", res.status());
        }
        let v: Value = res.json().await?;
        let list = v["installations"].as_array().cloned().unwrap_or_default();
        ids.extend(installation_ids(&v));
        if list.len() < 100 {
            break;
        }
    }
    Ok(ids)
}

/// The ids in GitHub's `GET /user/installations` answer.
fn installation_ids(v: &Value) -> Vec<i64> {
    v["installations"]
        .as_array()
        .map(|l| l.iter().filter_map(|i| i["id"].as_i64()).collect())
        .unwrap_or_default()
}

// ── Tokens and API calls ─────────────────────────────────────────────────────

/// A JWT signed with the app's key, for calls as the app itself.
fn app_jwt(s: &AppState, app: &GithubApp) -> anyhow::Result<String> {
    let pem = s.sealer.open(&app.pem_sealed)?;
    let now = pyo_proto::now_ms() / 1000;
    let claims = json!({ "iat": now - 60, "exp": now + 540, "iss": app.app_id.to_string() });
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())?;
    Ok(jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )?)
}

async fn app_get(s: &AppState, path: &str) -> anyhow::Result<Value> {
    let app = app(&s.db)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?
        .ok_or_else(|| anyhow::anyhow!("GitHub isn't set up"))?;
    let res = http()
        .get(format!("{API}{path}"))
        .bearer_auth(app_jwt(s, &app)?)
        .header("accept", "application/vnd.github+json")
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("GitHub answered {}", res.status());
    }
    Ok(res.json().await?)
}

fn tokens() -> &'static Mutex<HashMap<i64, (Instant, String)>> {
    static T: OnceLock<Mutex<HashMap<i64, (Instant, String)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An installation token (valid an hour; reused for 50 minutes).
pub async fn installation_token(s: &AppState, installation_id: i64) -> anyhow::Result<String> {
    if let Some((t, tok)) = tokens().lock().await.get(&installation_id)
        && t.elapsed() < Duration::from_secs(3000)
    {
        return Ok(tok.clone());
    }
    let app = app(&s.db)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?
        .ok_or_else(|| anyhow::anyhow!("GitHub isn't set up"))?;
    let res = http()
        .post(format!(
            "{API}/app/installations/{installation_id}/access_tokens"
        ))
        .bearer_auth(app_jwt(s, &app)?)
        .header("accept", "application/vnd.github+json")
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!(
            "GitHub refused a token for installation {installation_id} ({})",
            res.status()
        );
    }
    let v: Value = res.json().await?;
    let tok = v["token"].as_str().unwrap_or("").to_owned();
    tokens()
        .lock()
        .await
        .insert(installation_id, (Instant::now(), tok.clone()));
    Ok(tok)
}

async fn get_as(
    s: &AppState,
    installation_id: i64,
    path: &str,
    accept: &str,
) -> anyhow::Result<reqwest::Response> {
    let tok = installation_token(s, installation_id).await?;
    Ok(http()
        .get(format!("{API}{path}"))
        .bearer_auth(tok)
        .header("accept", accept)
        .send()
        .await?)
}

/// A file of a repository the team's GitHub installations can see: its text,
/// and the installation that read it. None when it isn't there.
pub async fn read_file(
    s: &AppState,
    team_id: &str,
    repo: &str,
    branch: &str,
    path: &str,
) -> ApiResult<Option<(String, i64)>> {
    let installs: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM pyo.github_installations WHERE team_id = $1 ORDER BY id",
    )
    .bind(team_id)
    .fetch_all(&s.db)
    .await?;
    if installs.is_empty() {
        return Err(ApiError::bad_request("Connect GitHub to this team first."));
    }
    let path = path.trim_start_matches('/');
    let encoded: String = path
        .split('/')
        .map(|p| {
            p.replace(' ', "%20")
                .replace('#', "%23")
                .replace('?', "%3F")
        })
        .collect::<Vec<_>>()
        .join("/");
    for id in installs {
        let res = get_as(
            s,
            id,
            &format!(
                "/repos/{repo}/contents/{encoded}?ref={}",
                branch.replace('&', "%26")
            ),
            "application/vnd.github.raw",
        )
        .await
        .map_err(ApiError::from)?;
        match res.status().as_u16() {
            200 => {
                let bytes = res.bytes().await.map_err(anyhow::Error::from)?;
                if bytes.len() > 512 * 1024 {
                    return Err(ApiError::bad_request(format!(
                        "{path} is too big to import."
                    )));
                }
                return Ok(Some((String::from_utf8_lossy(&bytes).into_owned(), id)));
            }
            404 => continue,
            code => {
                return Err(ApiError::new(
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("GitHub answered {code} for {path}."),
                ));
            }
        }
    }
    Ok(None)
}

/// "owner/repo" of a GitHub URL.
pub fn full_name(repo_url: &str) -> Option<String> {
    let rest = repo_url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let rest = rest
        .strip_prefix("https://github.com/")
        .or_else(|| rest.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let (o, r) = (parts.next()?, parts.next()?);
    (!o.is_empty() && !r.is_empty() && parts.next().is_none()).then(|| format!("{o}/{r}"))
}

/// The commit a branch points at.
pub async fn branch_head(
    s: &AppState,
    installation_id: i64,
    repo: &str,
    branch: &str,
) -> anyhow::Result<String> {
    let res = get_as(
        s,
        installation_id,
        &format!("/repos/{repo}/commits/{branch}"),
        "application/vnd.github.sha",
    )
    .await?;
    if !res.status().is_success() {
        anyhow::bail!(
            "{repo} has no branch {branch} that this installation can see ({})",
            res.status()
        );
    }
    Ok(res.text().await?.trim().to_owned())
}

/// GitHub repositories
///
/// The repositories the team's GitHub installations can see, to deploy
/// from.
#[utoipa::path(get, path = "/github/repos", operation_id = "listGithubRepos", tag = "GitHub",
    params(TeamQuery),
    responses(
        (status = 200, description = "`repositories`: each with `installationId`, `account`, `fullName`, `name`, `private`, `defaultBranch`, `htmlUrl` and `pushedAt`.", body = Object),
        (status = 400, description = "No `team`.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn repos(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<TeamQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    let installs: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, account FROM pyo.github_installations WHERE team_id = $1 ORDER BY account",
    )
    .bind(&q.team)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut out = vec![];
    for (id, account) in installs {
        let mut page = 1;
        loop {
            let res = get_as(
                &s,
                id,
                &format!("/installation/repositories?per_page=100&page={page}"),
                "application/vnd.github+json",
            )
            .await
            .map_err(ApiError::from)?;
            let v: Value = res.json().await.map_err(anyhow::Error::from)?;
            let list = v["repositories"].as_array().cloned().unwrap_or_default();
            for r in &list {
                out.push(json!({
                    "installationId": id, "account": account,
                    "fullName": r["full_name"], "name": r["name"], "private": r["private"],
                    "defaultBranch": r["default_branch"], "htmlUrl": r["html_url"], "pushedAt": r["pushed_at"],
                }));
            }
            if list.len() < 100 || page >= 10 {
                break;
            }
            page += 1;
        }
    }
    Ok(Json(json!({ "repositories": out })))
}

/// The file a Git build of a repository would use by default: its Dockerfile,
/// else its Containerfile, in the build context. None: the build is
/// zero-config (Railpack).
#[utoipa::path(get, path = "/github/build-file", operation_id = "getGithubBuildFile", tag = "GitHub",
    params(BuildFileQuery),
    responses(
        (status = 200, description = "`file`: \"Dockerfile\" or \"Containerfile\" (a Dockerfile wins when there are both), or null when there's neither.", body = Object),
        (status = 400, description = "No `team`, `repo` or `branch`, or the team hasn't connected GitHub.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn build_file(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<BuildFileQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    tx.commit().await?;
    let repo =
        full_name(&q.repo).unwrap_or_else(|| q.repo.trim().trim_end_matches(".git").to_owned());
    let dir = build_dir(&q.context)
        .ok_or_else(|| ApiError::bad_request("Build context can't contain '..'."))?;
    if repo.split('/').count() != 2 || q.branch.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Give the repository as owner/name, and a branch.",
        ));
    }
    for f in pyo_proto::BUILD_FILES {
        if read_file(&s, &q.team, &repo, q.branch.trim(), &format!("{dir}{f}"))
            .await?
            .is_some()
        {
            return Ok(Json(json!({ "file": f })));
        }
    }
    Ok(Json(json!({ "file": null })))
}

/// The build context as a prefix for its files ("" for the repository's
/// root, "apps/web/" for a folder). None when it leaves the repository.
fn build_dir(context: &str) -> Option<String> {
    let d = context.trim().trim_start_matches("./").trim_matches('/');
    if d.split('/').any(|p| p == "..") {
        return None;
    }
    Some(if d.is_empty() || d == "." {
        String::new()
    } else {
        format!("{d}/")
    })
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct BuildFileQuery {
    /// The team's id.
    team: String,
    /// The repository, as owner/name or its GitHub URL.
    repo: String,
    /// The branch to look at.
    branch: String,
    /// The build context, a folder of the repository ("." for its root).
    #[serde(default)]
    context: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct TeamQuery {
    /// The team's id.
    team: String,
}

// ── Commit statuses ──────────────────────────────────────────────────────────

/// Report a deployment on its commit (run as the "github.status" job).
/// Deployments not from a GitHub App repository have nothing to report.
pub async fn report(
    s: &AppState,
    deployment_id: &str,
    state: &str,
    description: &str,
) -> anyhow::Result<()> {
    let row: Option<(Value, String)> = sqlx::query_as("SELECT d.data, t.slug FROM pyo.deployments d JOIN pyo.teams t ON t.id = d.team_id WHERE d.id = $1")
        .bind(deployment_id)
        .fetch_optional(&s.db)
        .await?;
    let Some((data, slug)) = row else {
        return Ok(());
    };
    let src = &data["source"];
    let (Some(inst), Some(repo), Some(sha)) = (
        src["installationId"].as_i64(),
        full_name(src["repoUrl"].as_str().unwrap_or("")),
        src["commit"].as_str().filter(|c| c.len() == 40),
    ) else {
        return Ok(());
    };
    let Some(app) = app(&s.db)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?
    else {
        return Ok(());
    };
    let tok = installation_token(s, inst).await?;
    let res = http()
        .post(format!("{API}/repos/{repo}/statuses/{sha}"))
        .bearer_auth(tok)
        .header("accept", "application/vnd.github+json")
        .json(&json!({ "state": state, "description": description.chars().take(140).collect::<String>(), "context": "Pyo", "target_url": format!("{}/{slug}/deployments/{deployment_id}", app.public_url) }))
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("GitHub answered {} to the commit status", res.status());
    }
    Ok(())
}

// ── Pushes ───────────────────────────────────────────────────────────────────

/// Verify GitHub's "sha256=<hex>" signature of a webhook body.
pub fn signature_ok(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hexsig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig) = hex::decode(hexsig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

async fn webhook(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let Some(app) = app(&s.db).await? else {
        return Ok((StatusCode::NOT_FOUND, "GitHub isn't set up"));
    };
    let secret = s
        .sealer
        .open(&app.webhook_secret_sealed)
        .map_err(ApiError::from)?;
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !signature_ok(&secret, &body, sig) {
        return Ok((StatusCode::UNAUTHORIZED, "bad signature"));
    }
    if headers.get("x-github-event").and_then(|v| v.to_str().ok()) != Some("push") {
        return Ok((StatusCode::OK, "ignored"));
    }
    let v: Value = serde_json::from_slice(&body).map_err(anyhow::Error::from)?;
    if v["deleted"] == json!(true) {
        return Ok((StatusCode::OK, "branch deleted"));
    }
    let (Some(repo), Some(branch), Some(sha)) = (
        v["repository"]["full_name"].as_str(),
        v["ref"]
            .as_str()
            .and_then(|r| r.strip_prefix("refs/heads/")),
        v["after"].as_str(),
    ) else {
        return Ok((StatusCode::OK, "ignored"));
    };
    let pusher = v["pusher"]["name"].as_str().unwrap_or("GitHub").to_owned();
    let n = deploy_push(&s, repo, branch, sha, &pusher).await?;
    Ok((
        StatusCode::OK,
        if n > 0 {
            "deploying"
        } else {
            "no matching services"
        },
    ))
}

/// Deploy `sha` to every service that follows `repo`@`branch` with deploy on
/// push. Skips services pinned to a commit, rolled back, already on `sha`, or
/// mid-deploy (the poller picks those up once they're free).
pub async fn deploy_push(
    s: &AppState,
    repo: &str,
    branch: &str,
    sha: &str,
    actor: &str,
) -> ApiResult<usize> {
    let services: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT a.id, a.team_id, a.spec->'source' FROM pyo.services a
          WHERE NOT a.deleting AND a.spec->'source'->>'type' = 'git' AND a.spec->'source'->>'installationId' IS NOT NULL
            AND coalesce(a.spec->'source'->>'branch', 'main') = $1 AND coalesce((a.spec->'source'->>'autoDeploy')::bool, true)
            AND coalesce(a.spec->'source'->>'commit', '') = ''",
    )
    .bind(branch)
    .fetch_all(&s.db)
    .await?;
    let mut started = 0;
    for (service_id, team_id, src) in services {
        if full_name(src["repoUrl"].as_str().unwrap_or(""))
            .is_none_or(|r| !r.eq_ignore_ascii_case(repo))
        {
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
                    "github.status",
                    crate::jobs::github_status(&dep, "pending", "Building…"),
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

/// Without webhooks (Pyo isn't public), check each followed branch every minute.
pub async fn poller(s: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        crate::bg::as_leader(&s.db, crate::bg::LOCK_GITHUB, || poll_once(&s)).await;
    }
}

async fn poll_once(s: &AppState) {
    {
        let Ok(Some(app)) = app(&s.db).await else {
            return;
        };
        if app.webhook_active {
            return;
        }
        let rows: Result<Vec<(i64, String, String)>, _> = sqlx::query_as(
            "SELECT DISTINCT (a.spec->'source'->>'installationId')::bigint, a.spec->'source'->>'repoUrl', coalesce(a.spec->'source'->>'branch', 'main')
               FROM pyo.services a
              WHERE NOT a.deleting AND a.current_deployment_id IS NOT NULL AND a.spec->'source'->>'installationId' IS NOT NULL
                AND coalesce((a.spec->'source'->>'autoDeploy')::bool, true) AND coalesce(a.spec->'source'->>'commit', '') = ''",
        )
        .fetch_all(&s.db)
        .await;
        for (inst, url, branch) in rows.unwrap_or_default() {
            let Some(repo) = full_name(&url) else {
                continue;
            };
            match branch_head(s, inst, &repo, &branch).await {
                Ok(sha) if sha.len() == 40 => {
                    if let Err(e) = deploy_push(s, &repo, &branch, &sha, "GitHub").await {
                        tracing::warn!(%repo, error = ?e.message, "push poll");
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::debug!(%repo, error = %format!("{e:#}"), "branch head"),
            }
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/created", get(created))
        .route("/installed", get(installed))
        .route("/authorized", get(authorized))
        .route("/webhook", post(webhook))
}

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/github/repos", get(repos))
        .route("/github/build-file", get(build_file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_dirs() {
        assert_eq!(build_dir("").as_deref(), Some(""));
        assert_eq!(build_dir(".").as_deref(), Some(""));
        assert_eq!(build_dir("./").as_deref(), Some(""));
        assert_eq!(build_dir("./apps/web/").as_deref(), Some("apps/web/"));
        assert_eq!(build_dir("/apps/web").as_deref(), Some("apps/web/"));
        assert_eq!(build_dir("../x"), None);
        assert_eq!(build_dir("apps/../.."), None);
    }

    #[test]
    fn repo_names() {
        assert_eq!(
            full_name("https://github.com/upsetdev/whoami").as_deref(),
            Some("upsetdev/whoami")
        );
        assert_eq!(
            full_name("https://github.com/upsetdev/whoami.git/").as_deref(),
            Some("upsetdev/whoami")
        );
        assert_eq!(full_name("https://gitlab.com/a/b"), None);
        assert_eq!(full_name("https://github.com/a/b/tree/main"), None);
    }

    #[test]
    fn webhook_signatures() {
        // GitHub's documented example.
        let ok = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(signature_ok(
            "It's a Secret to Everybody",
            b"Hello, World!",
            ok
        ));
        assert!(!signature_ok("wrong", b"Hello, World!", ok));
        assert!(!signature_ok(
            "It's a Secret to Everybody",
            b"Hello, World!",
            "sha1=abc"
        ));
    }

    #[test]
    fn sign_in_goes_back_to_this_installation() {
        let app = GithubApp {
            client_id: "Iv1.abc".into(),
            public_url: "https://pyo.example.com/".into(),
            ..Default::default()
        };
        let url = reqwest::Url::parse(&authorize_url(&app, "st&ate")).unwrap();
        assert_eq!(url.host_str(), Some("github.com"));
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "Iv1.abc");
        assert_eq!(
            q["redirect_uri"],
            "https://pyo.example.com/api/github/authorized"
        );
        assert_eq!(q["state"], "st&ate");
    }

    #[test]
    fn installations_a_user_can_see() {
        let v = json!({ "total_count": 2, "installations": [{ "id": 7, "account": { "login": "a" } }, { "id": 9 }] });
        assert_eq!(installation_ids(&v), vec![7, 9]);
        assert!(installation_ids(&json!({ "message": "Bad credentials" })).is_empty());
    }

    #[tokio::test]
    async fn an_installation_link_needs_its_state() {
        let st = remember(Pending::Authorize {
            user_id: "u".into(),
            team_id: "t".into(),
            installation_id: 1,
        })
        .await;
        assert!(matches!(
            take(&st).await,
            Some(Pending::Authorize {
                installation_id: 1,
                ..
            })
        ));
        assert!(take(&st).await.is_none(), "single use");
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): an
    /// installation linked to one team can't be moved to another.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn an_installation_stays_with_its_team() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = crate::ids::new_id("t");
        let (a, b) = (format!("ta_{run}"), format!("tb_{run}"));
        let mut tx = pool.begin().await.unwrap();
        for t in [&a, &b] {
            sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(t)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        let id = 900_000_000 + (pyo_proto::now_ms() % 1_000_000);
        assert!(
            link_installation(&mut tx, id, &a, "acme", "Organization", "all")
                .await
                .unwrap()
        );
        assert!(
            !link_installation(&mut tx, id, &b, "acme", "Organization", "all")
                .await
                .unwrap(),
            "another team's"
        );
        assert!(
            link_installation(&mut tx, id, &a, "acme-renamed", "Organization", "selected")
                .await
                .unwrap(),
            "refresh"
        );
        let (team, account): (String, String) =
            sqlx::query_as("SELECT team_id, account FROM pyo.github_installations WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(
            (team.as_str(), account.as_str()),
            (a.as_str(), "acme-renamed")
        );
        tx.rollback().await.unwrap();
    }

    #[test]
    fn webhooks_need_a_public_https_url() {
        assert!(reachable("https://pyo.example.com"));
        assert!(!reachable("http://pyo.example.com"));
        assert!(!reachable("https://localhost:8000"));
        assert!(!reachable("https://100.64.0.10"));
    }
}
