//! Previews on GitHub, through Pyo's GitHub App (it shows as
//! `<app name>[bot]`): pull request events, the comment, the check run
//! "Pyo · Preview" and a deployment per preview ("View deployment").
//!
//! The app needs Pull requests, Deployments and Checks (write) and the
//! Pull request event. An app made before previews doesn't have them: its
//! owner adds them in the app's settings, then each installation's owner
//! approves them once on GitHub. Pyo asks GitHub what each has
//! ([`check_all`]) and the dashboard links where to do it.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Pull, PullRow, Remote, bot};
use crate::AppState;

/// What previews need, as GitHub names the permissions, and as people
/// read them.
const NEEDED: [(&str, &str); 3] = [
    ("pull_requests", "Pull requests: write"),
    ("deployments", "Deployments: write"),
    ("checks", "Checks: write"),
];

/// The permissions a new app asks for (github.rs `manifest`).
pub fn manifest_permissions() -> Value {
    json!({
        "contents": "read", "metadata": "read", "statuses": "write",
        "pull_requests": "write", "deployments": "write", "checks": "write",
    })
}

/// What's missing for previews, from what GitHub said the app (or an
/// installation) may do. Nothing while it hasn't been asked yet.
pub fn missing(permissions: &Value, events: &[String]) -> Vec<String> {
    if !permissions.is_object() {
        return vec![];
    }
    let mut out: Vec<String> = NEEDED
        .iter()
        .filter(|(k, _)| permissions[*k] != json!("write"))
        .map(|(_, shown)| (*shown).to_owned())
        .collect();
    if !events.iter().any(|e| e == "pull_request") {
        out.push("Pull request events".into());
    }
    out
}

/// Where the app's owner changes its permissions and events.
pub fn app_settings_url(app: &crate::github::GithubApp) -> String {
    if app.slug.is_empty() {
        return String::new();
    }
    match app.owner_type.as_str() {
        "Organization" => format!(
            "https://github.com/organizations/{}/settings/apps/{}/permissions",
            app.owner, app.slug
        ),
        _ => format!("https://github.com/settings/apps/{}/permissions", app.slug),
    }
}

/// Where an installation's owner approves new permissions.
pub fn installation_url(account: &str, account_type: &str, id: i64) -> String {
    match account_type {
        "Organization" => {
            format!("https://github.com/organizations/{account}/settings/installations/{id}")
        }
        _ => format!("https://github.com/settings/installations/{id}"),
    }
}

/// An installation in the snapshot: `missingPermissions` and `approveUrl`
/// instead of what GitHub said.
pub fn show_installation(i: &mut Value) {
    let events: Vec<String> = serde_json::from_value(i["events"].clone()).unwrap_or_default();
    let missing = missing(&i["permissions"], &events);
    let url = installation_url(
        i["account"].as_str().unwrap_or(""),
        i["accountType"].as_str().unwrap_or(""),
        i["id"].as_i64().unwrap_or(0),
    );
    if let Some(o) = i.as_object_mut() {
        o.remove("permissions");
        o.remove("events");
        o.insert("missingPermissions".into(), json!(missing));
        o.insert("approveUrl".into(), json!(url));
    }
}

/// Asks GitHub what one installation may do now, and keeps it.
pub async fn check_installation(s: &AppState, id: i64) {
    match crate::github::app_get(s, &format!("/app/installations/{id}")).await {
        Ok(v) => {
            let events: Vec<String> =
                serde_json::from_value(v["events"].clone()).unwrap_or_default();
            let r = sqlx::query("UPDATE pyo.github_installations SET permissions = $2, events = $3, checked_at = now() WHERE id = $1")
                .bind(id)
                .bind(&v["permissions"])
                .bind(&events)
                .execute(&s.db)
                .await;
            if let Err(e) = r {
                tracing::warn!(installation = id, error = %e, "previews: couldn't keep the installation's permissions");
            }
        }
        Err(e) => {
            tracing::info!(installation = id, error = %format!("{e:#}"), "previews: couldn't ask GitHub about an installation")
        }
    }
}

/// Asks GitHub what the app and each linked installation may do.
pub async fn check_all(s: &AppState) {
    let Ok(Some(_)) = crate::github::app(&s.db).await else {
        return;
    };
    match crate::github::app_get(s, "/app").await {
        Ok(v) => {
            let fields = json!({
                "permissions": v["permissions"],
                "events": v["events"],
                "ownerType": v["owner"]["type"],
            });
            let r = sqlx::query("UPDATE pyo.settings SET value = value || $1 WHERE key = 'github'")
                .bind(fields)
                .execute(&s.db)
                .await;
            if let Err(e) = r {
                tracing::warn!(error = %e, "previews: couldn't keep the app's permissions");
            }
        }
        Err(e) => {
            tracing::info!(error = %format!("{e:#}"), "previews: couldn't ask GitHub about the app")
        }
    }
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM pyo.github_installations")
        .fetch_all(&s.db)
        .await
        .unwrap_or_default();
    for id in ids {
        check_installation(s, id).await;
    }
}

// ── Events ─────────────────────────────────────────────────────────────────

/// A pull request from GitHub's API or a `pull_request` event (its
/// `pull_request` object). None when it's not one Pyo can use.
pub fn pull_of(pr: &Value, installation_id: i64) -> Option<Pull> {
    let base_repo = pr["base"]["repo"]["full_name"].as_str()?;
    let head_repo = pr["head"]["repo"]["full_name"].as_str();
    let state = match (
        pr["state"].as_str(),
        pr["merged"].as_bool(),
        pr["merged_at"].is_string(),
    ) {
        (Some("open"), _, _) => "open",
        (_, Some(true), _) | (_, _, true) => "merged",
        _ => "closed",
    };
    Some(Pull {
        provider: "github",
        installation_id: Some(installation_id),
        connection_id: None,
        repo: base_repo.to_owned(),
        number: pr["number"].as_i64()?,
        title: pr["title"].as_str().unwrap_or("").to_owned(),
        url: pr["html_url"].as_str().unwrap_or("").to_owned(),
        author: pr["user"]["login"].as_str().unwrap_or("").to_owned(),
        head_ref: pr["head"]["ref"].as_str().unwrap_or("").to_owned(),
        head_sha: pr["head"]["sha"]
            .as_str()
            .filter(|s| s.len() == 40)?
            .to_owned(),
        base_ref: pr["base"]["ref"].as_str().unwrap_or("").to_owned(),
        // A deleted fork has no repository any more: still a fork.
        fork: head_repo.is_none_or(|h| !h.eq_ignore_ascii_case(base_repo)),
        labels: pr["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(str::to_owned))
            .collect(),
        state,
    })
}

/// The actions of a `pull_request` event that change a preview.
const ACTIONS: [&str; 8] = [
    "opened",
    "reopened",
    "synchronize",
    "closed",
    "labeled",
    "unlabeled",
    "edited",
    "ready_for_review",
];

/// A `pull_request` event (signature already checked): its previews
/// follow. What to answer GitHub.
pub async fn webhook(s: &AppState, v: &Value) -> crate::error::ApiResult<&'static str> {
    if !ACTIONS.contains(&v["action"].as_str().unwrap_or("")) {
        return Ok("ignored");
    }
    let Some(inst) = v["installation"]["id"].as_i64() else {
        return Ok("no installation");
    };
    let Some(pull) = pull_of(&v["pull_request"], inst) else {
        return Ok("ignored");
    };
    let team: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM pyo.github_installations WHERE id = $1")
            .bind(inst)
            .fetch_optional(&s.db)
            .await?;
    let Some(team) = team else {
        return Ok("no team");
    };
    Ok(if super::pull_event(s, &team, &pull).await? {
        "previews"
    } else {
        "no previews"
    })
}

// ── The API ────────────────────────────────────────────────────────────────

/// GitHub's REST API as an installation.
pub struct Client {
    base: String,
    token: String,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Client {
        Client {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    pub async fn for_installation(s: &AppState, installation_id: i64) -> anyhow::Result<Client> {
        let token = crate::github::installation_token(s, installation_id).await?;
        Ok(Client::new(&crate::github::api_base(), &token))
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let mut req = crate::github::http()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req.send().await?;
        let status = res.status();
        let v = res.json().await.unwrap_or(Value::Null);
        Ok((status, v))
    }

    /// A call that has to work: GitHub's own words when it doesn't.
    async fn ok(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        what: &str,
    ) -> anyhow::Result<Value> {
        let (status, v) = self.call(method, path, body).await?;
        if !status.is_success() {
            let msg = v["message"].as_str().unwrap_or("");
            if status == StatusCode::FORBIDDEN
                || status == StatusCode::NOT_FOUND && msg.contains("Resource not accessible")
            {
                anyhow::bail!(
                    "GitHub refused to {what} ({status}): the app's new permissions may not be approved yet. {msg}"
                );
            }
            anyhow::bail!("GitHub refused to {what} ({status}). {msg}");
        }
        Ok(v)
    }

    /// The repository's open pull requests, the 100 updated last.
    pub async fn open_pulls(&self, repo: &str, installation_id: i64) -> anyhow::Result<Vec<Pull>> {
        let v = self
            .ok(
                Method::GET,
                &format!("/repos/{repo}/pulls?state=open&sort=updated&direction=desc&per_page=100"),
                None,
                "list pull requests",
            )
            .await?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| pull_of(p, installation_id))
            .collect())
    }

    pub async fn pull(
        &self,
        repo: &str,
        number: i64,
        installation_id: i64,
    ) -> anyhow::Result<Option<Pull>> {
        let (status, v) = self
            .call(Method::GET, &format!("/repos/{repo}/pulls/{number}"), None)
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("GitHub answered {status} for pull request {number}");
        }
        Ok(pull_of(&v, installation_id))
    }

    /// Posts the comment, or edits it; one deleted since is posted again.
    pub async fn comment(
        &self,
        repo: &str,
        number: i64,
        id: Option<i64>,
        body: &str,
    ) -> anyhow::Result<i64> {
        let b = json!({ "body": body });
        if let Some(id) = id {
            let (status, v) = self
                .call(
                    Method::PATCH,
                    &format!("/repos/{repo}/issues/comments/{id}"),
                    Some(&b),
                )
                .await?;
            if status.is_success() {
                return Ok(v["id"].as_i64().unwrap_or(id));
            }
            if status != StatusCode::NOT_FOUND {
                anyhow::bail!(
                    "GitHub refused to edit the comment ({status}). {}",
                    v["message"].as_str().unwrap_or("")
                );
            }
        }
        let v = self
            .ok(
                Method::POST,
                &format!("/repos/{repo}/issues/{number}/comments"),
                Some(&b),
                "comment on the pull request",
            )
            .await?;
        v["id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("GitHub gave the comment no id"))
    }

    pub async fn check(&self, repo: &str, id: Option<i64>, body: &Value) -> anyhow::Result<i64> {
        if let Some(id) = id {
            let mut b = body.clone();
            if let Some(o) = b.as_object_mut() {
                o.remove("head_sha");
            }
            let (status, v) = self
                .call(
                    Method::PATCH,
                    &format!("/repos/{repo}/check-runs/{id}"),
                    Some(&b),
                )
                .await?;
            if status.is_success() {
                return Ok(id);
            }
            if status != StatusCode::NOT_FOUND {
                anyhow::bail!(
                    "GitHub refused to update the check ({status}). {}",
                    v["message"].as_str().unwrap_or("")
                );
            }
        }
        let v = self
            .ok(
                Method::POST,
                &format!("/repos/{repo}/check-runs"),
                Some(body),
                "add the check",
            )
            .await?;
        v["id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("GitHub gave the check no id"))
    }

    /// A deployment of `sha` to `environment`: transient (GitHub doesn't
    /// mark other pull requests' previews inactive) and not production.
    pub async fn create_deployment(
        &self,
        repo: &str,
        sha: &str,
        environment: &str,
        description: &str,
    ) -> anyhow::Result<i64> {
        let v = self
            .ok(
                Method::POST,
                &format!("/repos/{repo}/deployments"),
                Some(&json!({
                    "ref": sha, "environment": environment, "description": description,
                    "auto_merge": false, "required_contexts": [],
                    "transient_environment": true, "production_environment": false,
                })),
                "add a deployment",
            )
            .await?;
        v["id"].as_i64().ok_or_else(|| {
            anyhow::anyhow!(
                "GitHub gave the deployment no id ({})",
                v["message"].as_str().unwrap_or("")
            )
        })
    }

    pub async fn deployment_status(
        &self,
        repo: &str,
        id: i64,
        state: &str,
        url: Option<&str>,
        logs: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut b = json!({ "state": state, "auto_inactive": false });
        if let Some(u) = url.filter(|_| state != "inactive") {
            b["environment_url"] = json!(u);
        }
        if let Some(l) = logs {
            b["log_url"] = json!(l);
        }
        let (status, v) = self
            .call(
                Method::POST,
                &format!("/repos/{repo}/deployments/{id}/statuses"),
                Some(&b),
            )
            .await?;
        // A deployment deleted on GitHub: nothing left to say.
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        anyhow::bail!(
            "GitHub refused the deployment status ({status}). {}",
            v["message"].as_str().unwrap_or("")
        )
    }
}

/// The environment of a service's previews: "Preview – web".
pub fn environment(service: &str) -> String {
    format!("Preview – {service}")
}

/// The bot on one pull request.
pub struct Forge {
    client: Client,
    repo: String,
    number: i64,
    dashboard: String,
}

impl Forge {
    /// None without the app or an installation (it was removed).
    pub async fn for_pull(s: &AppState, pull: &PullRow) -> anyhow::Result<Option<Forge>> {
        let Some(app) = crate::github::app(&s.db)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?
        else {
            return Ok(None);
        };
        let Some(inst) = pull.installation_id else {
            return Ok(None);
        };
        Ok(Some(Forge {
            client: Client::for_installation(s, inst).await?,
            repo: pull.repo.clone(),
            number: pull.number,
            dashboard: app.public_url.trim_end_matches('/').to_owned(),
        }))
    }

    #[cfg(test)]
    pub fn new(client: Client, repo: &str, number: i64, dashboard: &str) -> Forge {
        Forge {
            client,
            repo: repo.to_owned(),
            number,
            dashboard: dashboard.to_owned(),
        }
    }

    /// Where Pyo's dashboard is, for logs links.
    pub fn dashboard(&self) -> &str {
        &self.dashboard
    }
}

impl super::Forge for Forge {
    async fn comment(&self, id: Option<i64>, body: &str) -> anyhow::Result<i64> {
        self.client.comment(&self.repo, self.number, id, body).await
    }

    async fn check(
        &self,
        id: Option<i64>,
        sha: &str,
        check: &bot::Check,
        details: Option<&str>,
    ) -> anyhow::Result<Option<i64>> {
        Ok(Some(
            self.client
                .check(&self.repo, id, &check.github(sha, details))
                .await?,
        ))
    }

    async fn deployment(&self, d: &mut Remote, shown: bot::Shown) -> anyhow::Result<()> {
        let state = shown.github_deployment();
        if state == "inactive" {
            if let Some(id) = d.deployment_id {
                self.client
                    .deployment_status(&self.repo, id, "inactive", None, d.logs.as_deref())
                    .await?;
            }
            return Ok(());
        }
        // A new head: a new deployment, and the one before goes inactive.
        if d.deployment_id.is_none() || d.deployment_sha.as_deref() != Some(d.sha.as_str()) {
            if let Some(old) = d.deployment_id.take() {
                self.client
                    .deployment_status(&self.repo, old, "inactive", None, None)
                    .await?;
            }
            let id = self
                .client
                .create_deployment(
                    &self.repo,
                    &d.sha,
                    &environment(&d.service),
                    &format!("Preview of {}", d.service),
                )
                .await?;
            d.deployment_id = Some(id);
            d.deployment_sha = Some(d.sha.clone());
        }
        if let Some(id) = d.deployment_id {
            self.client
                .deployment_status(&self.repo, id, state, d.url.as_deref(), d.logs.as_deref())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Forge as _;
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::{HeaderMap, StatusCode as Code},
        routing::{patch, post},
    };
    use std::sync::{Arc, Mutex};

    fn pr(state: &str, head_repo: Option<&str>) -> Value {
        json!({
            "number": 42, "state": state, "merged": state == "merged", "title": "Add search",
            "html_url": "https://github.com/acme/shop/pull/42", "user": { "login": "ana" },
            "head": { "ref": "search", "sha": "a".repeat(40), "repo": head_repo.map(|r| json!({ "full_name": r })) },
            "base": { "ref": "main", "repo": { "full_name": "acme/shop" } },
            "labels": [{ "name": "preview" }],
        })
    }

    #[test]
    fn pull_requests_from_events() {
        let p = pull_of(&pr("open", Some("acme/shop")), 7).unwrap();
        assert_eq!(
            (p.repo.as_str(), p.number, p.state, p.fork),
            ("acme/shop", 42, "open", false)
        );
        assert_eq!(p.labels, vec!["preview"]);
        assert_eq!(p.installation_id, Some(7));
        assert_eq!(p.head_ref, "search");
        // From a fork, or a fork deleted since.
        assert!(pull_of(&pr("open", Some("mallory/shop")), 7).unwrap().fork);
        assert!(pull_of(&pr("open", None), 7).unwrap().fork);
        // Closed, and merged (GitHub says "closed" with merged true).
        let mut merged = pr("closed", Some("acme/shop"));
        merged["merged"] = json!(true);
        assert_eq!(pull_of(&merged, 7).unwrap().state, "merged");
        assert_eq!(
            pull_of(&pr("closed", Some("acme/shop")), 7).unwrap().state,
            "closed"
        );
        // No head commit: not one to deploy.
        let mut bad = pr("open", Some("acme/shop"));
        bad["head"]["sha"] = json!("abc");
        assert!(pull_of(&bad, 7).is_none());
    }

    #[test]
    fn what_previews_still_need() {
        let all = json!({ "pull_requests": "write", "deployments": "write", "checks": "write", "contents": "read" });
        assert!(missing(&all, &["push".into(), "pull_request".into()]).is_empty());
        let old = json!({ "contents": "read", "metadata": "read", "statuses": "write" });
        assert_eq!(
            missing(&old, &["push".into()]),
            vec![
                "Pull requests: write",
                "Deployments: write",
                "Checks: write",
                "Pull request events"
            ]
        );
        let read = json!({ "pull_requests": "read", "deployments": "write", "checks": "write" });
        assert_eq!(
            missing(&read, &["pull_request".into()]),
            vec!["Pull requests: write"]
        );
        // Not asked yet: nothing to say.
        assert!(missing(&Value::Null, &[]).is_empty());
        // New apps ask for all of it.
        assert!(missing(&manifest_permissions(), &["pull_request".into()]).is_empty());
    }

    #[test]
    fn where_to_approve() {
        assert_eq!(
            installation_url("acme", "Organization", 9),
            "https://github.com/organizations/acme/settings/installations/9"
        );
        assert_eq!(
            installation_url("ana", "User", 9),
            "https://github.com/settings/installations/9"
        );
        let mut app = crate::github::GithubApp {
            slug: "pyo-abc".into(),
            owner: "acme".into(),
            ..Default::default()
        };
        assert_eq!(
            app_settings_url(&app),
            "https://github.com/settings/apps/pyo-abc/permissions"
        );
        app.owner_type = "Organization".into();
        assert_eq!(
            app_settings_url(&app),
            "https://github.com/organizations/acme/settings/apps/pyo-abc/permissions"
        );
        let mut i = json!({ "id": 9, "account": "acme", "accountType": "Organization", "permissions": { "contents": "read" }, "events": ["push"] });
        show_installation(&mut i);
        assert_eq!(
            i["approveUrl"],
            "https://github.com/organizations/acme/settings/installations/9"
        );
        assert_eq!(i["missingPermissions"].as_array().unwrap().len(), 4);
        assert!(i.get("permissions").is_none());
    }

    /// Requests the fake GitHub saw: method and path, then the body.
    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    #[derive(Clone)]
    struct Fake {
        seen: Seen,
        /// Comments that were deleted on GitHub.
        gone: Arc<Mutex<Vec<i64>>>,
    }

    fn authed(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer ghs_test")
    }

    async fn fake() -> (Client, Fake) {
        let f = Fake {
            seen: Seen::default(),
            gone: Arc::default(),
        };
        let app = Router::new()
            .route(
                "/repos/{o}/{r}/issues/{n}/comments",
                post(
                    |State(f): State<Fake>,
                     Path((_o, _r, n)): Path<(String, String, i64)>,
                     h: HeaderMap,
                     Json(b): Json<Value>| async move {
                        assert!(authed(&h));
                        f.seen
                            .lock()
                            .unwrap()
                            .push((format!("POST comment {n}"), b));
                        (Code::CREATED, Json(json!({ "id": 1000 + n })))
                    },
                ),
            )
            .route(
                "/repos/{o}/{r}/issues/comments/{id}",
                patch(
                    |State(f): State<Fake>,
                     Path((_o, _r, id)): Path<(String, String, i64)>,
                     Json(b): Json<Value>| async move {
                        f.seen
                            .lock()
                            .unwrap()
                            .push((format!("PATCH comment {id}"), b));
                        if f.gone.lock().unwrap().contains(&id) {
                            return (Code::NOT_FOUND, Json(json!({ "message": "Not Found" })));
                        }
                        (Code::OK, Json(json!({ "id": id })))
                    },
                ),
            )
            .route(
                "/repos/{o}/{r}/check-runs",
                post(|State(f): State<Fake>, Json(b): Json<Value>| async move {
                    f.seen.lock().unwrap().push(("POST check".into(), b));
                    (Code::CREATED, Json(json!({ "id": 77 })))
                }),
            )
            .route(
                "/repos/{o}/{r}/check-runs/{id}",
                patch(
                    |State(f): State<Fake>,
                     Path((_o, _r, id)): Path<(String, String, i64)>,
                     Json(b): Json<Value>| async move {
                        f.seen
                            .lock()
                            .unwrap()
                            .push((format!("PATCH check {id}"), b));
                        (Code::OK, Json(json!({ "id": id })))
                    },
                ),
            )
            .route(
                "/repos/{o}/{r}/deployments",
                post(|State(f): State<Fake>, Json(b): Json<Value>| async move {
                    let n = f
                        .seen
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(k, _)| k == "POST deployment")
                        .count() as i64;
                    f.seen.lock().unwrap().push(("POST deployment".into(), b));
                    (Code::CREATED, Json(json!({ "id": 500 + n })))
                }),
            )
            .route(
                "/repos/{o}/{r}/deployments/{id}/statuses",
                post(
                    |State(f): State<Fake>,
                     Path((_o, _r, id)): Path<(String, String, i64)>,
                     Json(b): Json<Value>| async move {
                        f.seen
                            .lock()
                            .unwrap()
                            .push((format!("POST status {id}"), b));
                        (Code::CREATED, Json(json!({ "id": 1 })))
                    },
                ),
            )
            .with_state(f.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (Client::new(&format!("http://{addr}"), "ghs_test"), f)
    }

    fn seen(f: &Fake) -> Vec<String> {
        f.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }

    #[tokio::test]
    async fn the_comment_is_edited_in_place_and_posted_again_when_deleted() {
        let (c, f) = fake().await;
        let forge = Forge::new(c, "acme/shop", 42, "https://pyo.example.com");
        let id = forge.comment(None, "one").await.unwrap();
        assert_eq!(id, 1042);
        assert_eq!(forge.comment(Some(id), "two").await.unwrap(), 1042);
        f.gone.lock().unwrap().push(1042);
        assert_eq!(forge.comment(Some(1042), "three").await.unwrap(), 1042);
        assert_eq!(
            seen(&f),
            vec![
                "POST comment 42",
                "PATCH comment 1042",
                "PATCH comment 1042",
                "POST comment 42"
            ]
        );
        assert_eq!(f.seen.lock().unwrap()[3].1["body"], "three");
    }

    #[tokio::test]
    async fn the_check_is_made_once_per_head_then_updated() {
        let (c, f) = fake().await;
        let forge = Forge::new(c, "acme/shop", 42, "");
        let building = bot::Check {
            status: "in_progress",
            conclusion: None,
            title: "Building".into(),
            summary: String::new(),
        };
        let id = forge
            .check(None, &"a".repeat(40), &building, None)
            .await
            .unwrap();
        assert_eq!(id, Some(77));
        let done = bot::Check {
            status: "completed",
            conclusion: Some("failure"),
            title: "Failed: web".into(),
            summary: "- web: Failed".into(),
        };
        forge
            .check(id, &"a".repeat(40), &done, Some("https://pyo/x"))
            .await
            .unwrap();
        let s = f.seen.lock().unwrap().clone();
        assert_eq!(s[0].0, "POST check");
        assert_eq!(s[0].1["name"], "Pyo · Preview");
        assert_eq!(s[0].1["status"], "in_progress");
        assert_eq!(s[1].0, "PATCH check 77");
        assert_eq!(s[1].1["conclusion"], "failure");
        assert!(
            s[1].1.get("head_sha").is_none(),
            "an update can't move the check"
        );
    }

    #[tokio::test]
    async fn a_deployment_per_head_with_its_states() {
        let (c, f) = fake().await;
        let forge = Forge::new(c, "acme/shop", 42, "");
        let mut d = Remote {
            name: "web-pr-42".into(),
            service: "web".into(),
            sha: "a".repeat(40),
            url: Some("https://web-pr-42.203.0.113.7.nip.io".into()),
            logs: Some("https://pyo/acme/deployments/dpl_1".into()),
            ..Default::default()
        };
        forge
            .deployment(&mut d, bot::Shown::Building)
            .await
            .unwrap();
        forge.deployment(&mut d, bot::Shown::Ready).await.unwrap();
        // A push: a new deployment, the old one inactive.
        d.sha = "b".repeat(40);
        forge
            .deployment(&mut d, bot::Shown::Building)
            .await
            .unwrap();
        forge.deployment(&mut d, bot::Shown::Removed).await.unwrap();
        assert_eq!(
            seen(&f),
            vec![
                "POST deployment",
                "POST status 500",
                "POST status 500",
                "POST status 500",
                "POST deployment",
                "POST status 501",
                "POST status 501"
            ]
        );
        let s = f.seen.lock().unwrap().clone();
        assert_eq!(s[0].1["environment"], "Preview – web");
        assert_eq!(s[0].1["transient_environment"], true);
        assert_eq!(s[0].1["production_environment"], false);
        assert_eq!(s[1].1["state"], "in_progress");
        assert_eq!(s[2].1["state"], "success");
        assert_eq!(
            s[2].1["environment_url"],
            "https://web-pr-42.203.0.113.7.nip.io"
        );
        assert_eq!(s[3].1["state"], "inactive");
        assert_eq!(s[4].1["ref"], "b".repeat(40));
        assert_eq!(s[6].1["state"], "inactive");
        assert!(s[6].1.get("environment_url").is_none());
        assert_eq!(d.deployment_id, Some(501));
    }
}
