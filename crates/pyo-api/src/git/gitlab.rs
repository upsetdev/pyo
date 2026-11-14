//! GitLab's REST API (v4), as a team's GitLab connection uses it: on
//! gitlab.com or a self-managed GitLab at the address the team gives.
//!
//! Every call goes through Pyo's guarded client ([`crate::outbound`]): the
//! address is checked when the client is made and again by its resolver at
//! each connection, and redirects aren't followed. GitLab's answers are
//! never passed on whole, only their status and GitLab's own message.

use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde_json::{Value, json};

use crate::outbound::{self, Reach, Refused};

const TIMEOUT: Duration = Duration::from_secs(20);

/// The access a connection needs on a project: webhooks and deploy tokens
/// are a Maintainer's.
pub const MAINTAINER: u64 = 40;

/// Something GitLab (or the way to it) refused, in words for the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// GitLab's HTTP status, None when it wasn't reached.
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn error(status: Option<u16>, message: impl Into<String>) -> Error {
    Error {
        status,
        message: message.into(),
    }
}

/// What a status means for the person, with GitLab's own message when it
/// gave one (never the rest of the answer).
fn describe(status: StatusCode, what: &str, body: &Value) -> Error {
    let said = gitlab_message(body);
    let text = match status {
        StatusCode::UNAUTHORIZED => {
            "GitLab refused the token. It may have expired or been revoked.".to_owned()
        }
        StatusCode::FORBIDDEN => format!(
            "The token can't {what}. It needs the api scope and the Maintainer role."
        ),
        StatusCode::NOT_FOUND => format!("GitLab found nothing to {what}, or the token can't see it."),
        s if s.is_redirection() => "GitLab answered with a redirect, which Pyo doesn't follow. Use its final address, with https://.".to_owned(),
        s => format!("GitLab couldn't {what} ({}).", s.as_u16()),
    };
    error(
        Some(status.as_u16()),
        match said {
            Some(m) if !matches!(status.as_u16(), 401 | 404) => format!("{text} GitLab said: {m}"),
            _ => text,
        },
    )
}

/// GitLab's `message` or `error`, short and on one line.
fn gitlab_message(body: &Value) -> Option<String> {
    let m = match (&body["message"], &body["error"]) {
        (Value::String(s), _) | (_, Value::String(s)) => s.clone(),
        (Value::Object(_) | Value::Array(_), _) => body["message"].to_string(),
        _ => return None,
    };
    let m: String = m.split_whitespace().collect::<Vec<_>>().join(" ");
    (!m.is_empty()).then(|| m.chars().take(200).collect())
}

/// A GitLab project, as the repository picker lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: u64,
    pub path: String,
    pub name: String,
    pub namespace: String,
    pub private: bool,
    pub default_branch: String,
    pub web_url: String,
    pub last_activity_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub commit: String,
    pub default: bool,
    pub protected: bool,
}

/// What a token can do, for the Test button and when it's saved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Check {
    /// The token's user: a bot for group and project tokens.
    pub user: String,
    /// What it belongs to: the group or project it's for, else the user.
    pub account: String,
    /// Empty when GitLab didn't say (before 15.5).
    pub scopes: Vec<String>,
    pub expires_at: Option<String>,
    pub version: String,
    /// Projects it has the Maintainer role on (up to 100).
    pub projects: usize,
    /// What's missing, in words. Empty when it's all there.
    pub problems: Vec<String>,
    /// Problems that stop it from working at all.
    pub fatal: bool,
}

/// A short-lived token that can only read one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployToken {
    pub id: u64,
    pub username: String,
    pub token: String,
}

pub struct Gitlab {
    /// The GitLab address, without a trailing slash.
    base: Url,
    token: String,
    http: reqwest::Client,
}

/// A GitLab address as Pyo keeps it: http(s), a host, maybe a path (GitLab
/// under /gitlab), no login, query or trailing slash.
pub fn normalize(addr: &str) -> std::result::Result<Url, Refused> {
    let mut url = outbound::parse(addr)?;
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url)
}

/// `url` as text without a trailing slash.
pub fn base_text(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_owned()
}

impl Gitlab {
    /// A client for GitLab at `base`, checked against what it resolves to
    /// now ([`Reach::team`]: private networks unless the installation turns
    /// them off, never this machine or the cluster's own networks).
    pub async fn connect(base: &str, token: &str) -> Result<Self> {
        let base = normalize(base).map_err(|e| error(None, e.to_string()))?;
        outbound::check(&base, Reach::team())
            .await
            .map_err(|e| error(None, e.to_string()))?;
        let http = outbound::client(Reach::team(), TIMEOUT)
            .map_err(|_| error(None, "Pyo couldn't start an HTTP client."))?;
        Ok(Self::with_client(base, token, http))
    }

    /// With a client of its own (tests: a mock GitLab on this machine,
    /// which the guarded client would refuse).
    pub fn with_client(base: Url, token: &str, http: reqwest::Client) -> Self {
        Self {
            base,
            token: token.to_owned(),
            http,
        }
    }

    /// {base}/api/v4/{segments}, each segment encoded on its own (so a
    /// project's path is one segment, its slashes as %2F).
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut p) = url.path_segments_mut() {
            p.pop_if_empty().extend(["api", "v4"]).extend(segments);
        }
        url
    }

    fn host(&self) -> String {
        self.base.host_str().unwrap_or("GitLab").to_owned()
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(StatusCode, Value)> {
        let res = req
            .header("PRIVATE-TOKEN", &self.token)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                let refused =
                    std::iter::successors(Some(&e as &dyn std::error::Error), |e| e.source())
                        .find_map(|e| e.downcast_ref::<Refused>().cloned());
                error(
                    None,
                    match refused {
                        Some(r) => r.to_string(),
                        None => format!("Pyo can't reach {}.", self.host()),
                    },
                )
            })?;
        let status = res.status();
        let body = res.json().await.unwrap_or(Value::Null);
        Ok((status, body))
    }

    async fn get(&self, url: Url, what: &str) -> Result<Value> {
        let (status, body) = self.send(self.http.get(url)).await?;
        if !status.is_success() {
            return Err(describe(status, what, &body));
        }
        Ok(body)
    }

    /// Pages of a list, up to `pages` of 100.
    async fn list(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
        pages: u32,
        what: &str,
    ) -> Result<Vec<Value>> {
        let mut out = vec![];
        for page in 1..=pages {
            let mut url = self.url(segments);
            {
                let mut q = url.query_pairs_mut();
                q.extend_pairs(query);
                q.append_pair("per_page", "100");
                q.append_pair("page", &page.to_string());
            }
            let v = self.get(url, what).await?;
            let list = v.as_array().cloned().unwrap_or_default();
            let n = list.len();
            out.extend(list);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// What the token is and can do: everything the Test button reports.
    pub async fn check(&self) -> Result<Check> {
        let user = self.get(self.url(&["user"]), "read its own user").await?;
        let mut c = Check {
            user: user["username"].as_str().unwrap_or("").to_owned(),
            ..Default::default()
        };
        // Scopes and expiry (GitLab 15.5 and later).
        match self
            .send(self.http.get(self.url(&["personal_access_tokens", "self"])))
            .await?
        {
            (s, v) if s.is_success() => {
                c.scopes = v["scopes"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                c.expires_at = v["expires_at"].as_str().map(str::to_owned);
            }
            (StatusCode::UNAUTHORIZED, v) => {
                return Err(describe(StatusCode::UNAUTHORIZED, "", &v));
            }
            _ => {}
        }
        if let Ok(v) = self.get(self.url(&["version"]), "say its version").await {
            c.version = v["version"].as_str().unwrap_or("").to_owned();
        }
        let level = MAINTAINER.to_string();
        let projects =
            if c.scopes.is_empty() || c.scopes.iter().any(|s| s == "api" || s == "read_api") {
                self.list(
                    &["projects"],
                    &[
                        ("membership", "true"),
                        ("min_access_level", &level),
                        ("simple", "true"),
                        ("order_by", "last_activity_at"),
                    ],
                    1,
                    "list its projects",
                )
                .await?
            } else {
                vec![]
            };
        c.projects = projects.iter().filter(|p| project_of(p).is_some()).count();
        let groups = if c.projects > 0 {
            self.list(
                &["groups"],
                &[("min_access_level", &level)],
                1,
                "list its groups",
            )
            .await
            .unwrap_or_default()
        } else {
            vec![]
        };
        c.account = account_of(&groups, &projects).unwrap_or_else(|| c.user.clone());
        c.problems = problems(&c, &today());
        c.fatal = !c.scopes.is_empty() && !c.scopes.iter().any(|s| s == "api");
        Ok(c)
    }

    /// Projects the token has the Maintainer role on, latest activity first.
    pub async fn projects(&self, search: Option<&str>) -> Result<Vec<Project>> {
        let level = MAINTAINER.to_string();
        let mut q = vec![
            ("membership", "true"),
            ("min_access_level", level.as_str()),
            ("order_by", "last_activity_at"),
            ("archived", "false"),
        ];
        let search = search.map(str::trim).filter(|s| !s.is_empty());
        if let Some(s) = search {
            q.push(("search", s));
            q.push(("search_namespaces", "true"));
        }
        Ok(self
            .list(&["projects"], &q, 5, "list its projects")
            .await?
            .iter()
            .filter_map(project_of)
            .collect())
    }

    pub async fn branches(&self, repo: &str, search: Option<&str>) -> Result<Vec<Branch>> {
        let mut q = vec![];
        if let Some(s) = search.map(str::trim).filter(|s| !s.is_empty()) {
            q.push(("search", s));
        }
        Ok(self
            .list(
                &["projects", repo, "repository", "branches"],
                &q,
                3,
                &format!("list the branches of {repo}"),
            )
            .await?
            .iter()
            .map(|b| Branch {
                name: b["name"].as_str().unwrap_or("").to_owned(),
                commit: b["commit"]["id"].as_str().unwrap_or("").to_owned(),
                default: b["default"].as_bool().unwrap_or(false),
                protected: b["protected"].as_bool().unwrap_or(false),
            })
            .filter(|b| !b.name.is_empty())
            .collect())
    }

    /// The commit a branch points at.
    pub async fn branch_head(&self, repo: &str, branch: &str) -> Result<String> {
        let v = self
            .get(
                self.url(&["projects", repo, "repository", "branches", branch]),
                &format!("find branch {branch} of {repo}"),
            )
            .await?;
        v["commit"]["id"]
            .as_str()
            .filter(|c| c.len() == 40)
            .map(str::to_owned)
            .ok_or_else(|| error(None, format!("GitLab gave no commit for {branch}.")))
    }

    /// Whether a file is in the repository at `branch`.
    pub async fn file_exists(&self, repo: &str, branch: &str, path: &str) -> Result<bool> {
        let mut url = self.url(&["projects", repo, "repository", "files", path]);
        url.query_pairs_mut().append_pair("ref", branch);
        let (status, body) = self.send(self.http.head(url)).await?;
        match status.as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(describe(status, &format!("read {path}"), &body)),
        }
    }

    /// Set Pyo's status on a commit. A status already in that state is
    /// left as it is (GitLab refuses the same move twice).
    pub async fn set_status(
        &self,
        repo: &str,
        sha: &str,
        state: &str,
        description: &str,
        target_url: &str,
    ) -> Result<()> {
        let mut body = json!({
            "state": state,
            "name": "Pyo",
            "description": description.chars().take(140).collect::<String>(),
        });
        if !target_url.is_empty() {
            body["target_url"] = json!(target_url);
        }
        let (status, v) = self
            .send(
                self.http
                    .post(self.url(&["projects", repo, "statuses", sha]))
                    .json(&body),
            )
            .await?;
        if status.is_success() || (status == StatusCode::BAD_REQUEST && same_state(&v)) {
            return Ok(());
        }
        Err(describe(status, "set the commit status", &v))
    }

    /// A deploy token that can only read `repo`, expiring at `expires_at`
    /// (RFC 3339).
    pub async fn create_deploy_token(
        &self,
        repo: &str,
        name: &str,
        expires_at: &str,
    ) -> Result<DeployToken> {
        let (status, v) = self
            .send(
                self.http
                    .post(self.url(&["projects", repo, "deploy_tokens"]))
                    .json(&json!({ "name": name, "scopes": ["read_repository"], "expires_at": expires_at })),
            )
            .await?;
        if !status.is_success() {
            return Err(describe(
                status,
                &format!("make a deploy token for {repo}"),
                &v,
            ));
        }
        match (
            v["id"].as_u64(),
            v["username"].as_str(),
            v["token"].as_str(),
        ) {
            (Some(id), Some(u), Some(t)) if !t.is_empty() => Ok(DeployToken {
                id,
                username: u.to_owned(),
                token: t.to_owned(),
            }),
            _ => Err(error(None, "GitLab didn't return the deploy token.")),
        }
    }

    pub async fn delete_deploy_token(&self, repo: &str, id: u64) -> Result<()> {
        self.delete(
            self.url(&["projects", repo, "deploy_tokens", &id.to_string()]),
            "remove a deploy token",
        )
        .await
    }

    /// The project's webhooks: (id, url).
    pub async fn hooks(&self, repo: &str) -> Result<Vec<(u64, String)>> {
        Ok(self
            .list(
                &["projects", repo, "hooks"],
                &[],
                1,
                &format!("list the webhooks of {repo}"),
            )
            .await?
            .iter()
            .filter_map(|h| Some((h["id"].as_u64()?, h["url"].as_str()?.to_owned())))
            .collect())
    }

    /// A webhook for pushes, sending `secret` as its token.
    pub async fn create_hook(&self, repo: &str, url: &str, secret: &str) -> Result<u64> {
        let (status, v) = self
            .send(
                self.http
                    .post(self.url(&["projects", repo, "hooks"]))
                    .json(&json!({
                        "url": url,
                        "token": secret,
                        "push_events": true,
                        "tag_push_events": false,
                        "merge_requests_events": false,
                        "enable_ssl_verification": url.starts_with("https://"),
                    })),
            )
            .await?;
        if !status.is_success() {
            return Err(describe(status, &format!("add a webhook to {repo}"), &v));
        }
        v["id"]
            .as_u64()
            .ok_or_else(|| error(None, "GitLab didn't say which webhook it made."))
    }

    pub async fn delete_hook(&self, repo: &str, id: u64) -> Result<()> {
        self.delete(
            self.url(&["projects", repo, "hooks", &id.to_string()]),
            "remove a webhook",
        )
        .await
    }

    /// Gone already counts as done.
    async fn delete(&self, url: Url, what: &str) -> Result<()> {
        let (status, v) = self.send(self.http.delete(url)).await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(describe(status, what, &v))
    }
}

/// GitLab's answer when a status is already in the state it was asked for.
fn same_state(v: &Value) -> bool {
    gitlab_message(v).is_some_and(|m| m.contains("Cannot transition status"))
}

fn project_of(p: &Value) -> Option<Project> {
    Some(Project {
        id: p["id"].as_u64()?,
        path: p["path_with_namespace"].as_str()?.to_owned(),
        name: p["name"].as_str().unwrap_or("").to_owned(),
        namespace: p["namespace"]["full_path"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
        // Internal projects need a GitLab login too.
        private: p["visibility"].as_str() != Some("public"),
        default_branch: p["default_branch"].as_str().unwrap_or("").to_owned(),
        web_url: p["web_url"].as_str()?.to_owned(),
        last_activity_at: p["last_activity_at"].as_str().unwrap_or("").to_owned(),
    })
}

/// What a token is for: its top group (a group token), else its project (a
/// project token). None for neither (a user's own token).
fn account_of(groups: &[Value], projects: &[Value]) -> Option<String> {
    groups
        .iter()
        .filter_map(|g| g["full_path"].as_str())
        .min_by_key(|p| (p.matches('/').count(), p.len()))
        .or_else(|| {
            (projects.len() == 1)
                .then(|| projects[0]["path_with_namespace"].as_str())
                .flatten()
        })
        .map(str::to_owned)
}

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

/// What's missing from a token, in words.
fn problems(c: &Check, today: &str) -> Vec<String> {
    let mut out = vec![];
    if !c.scopes.is_empty() && !c.scopes.iter().any(|s| s == "api") {
        out.push(format!(
            "The token needs the api scope. It has {}.",
            c.scopes.join(", ")
        ));
    } else if c.projects == 0 {
        out.push(
            "The token has no project with the Maintainer role. Give it the Maintainer role."
                .to_owned(),
        );
    }
    if let Some(exp) = &c.expires_at
        && let (Ok(e), Ok(t)) = (
            exp.parse::<jiff::civil::Date>(),
            today.parse::<jiff::civil::Date>(),
        )
        && let Ok(left) = t.until(e)
        && left.get_days() <= 14
    {
        out.push(format!(
            "The token expires on {exp}. Replace it before then."
        ));
    }
    out
}

/// The commit status GitLab takes for Pyo's own: "pending" when a deploy
/// starts, "running" once a builder has it, then "success", "failure" or
/// "error" (cancelled).
pub fn status_state(state: &str) -> &'static str {
    match state {
        "running" => "running",
        "success" => "success",
        "failure" => "failed",
        "error" => "canceled",
        _ => "pending",
    }
}

/// Whether a webhook carries the connection's secret (`X-Gitlab-Token`).
pub fn token_ok(secret: &str, header: Option<&str>) -> bool {
    match header {
        Some(h) if !secret.is_empty() => {
            crate::util::constant_time_eq(h.as_bytes(), secret.as_bytes())
        }
        _ => false,
    }
}

/// A push that may deploy: the project's path, the branch, the new commit
/// and who pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Push {
    pub repo: String,
    pub branch: String,
    pub sha: String,
    pub actor: String,
}

/// A push event's body (`X-Gitlab-Event: Push Hook`). None for anything
/// else: tags, a deleted branch, other events.
pub fn push_of(event: Option<&str>, v: &Value) -> Option<Push> {
    if event != Some("Push Hook") || v["object_kind"].as_str() != Some("push") {
        return None;
    }
    let sha = v["after"].as_str().filter(|s| s.len() == 40)?;
    if sha.bytes().all(|b| b == b'0') {
        return None;
    }
    Some(Push {
        repo: v["project"]["path_with_namespace"].as_str()?.to_owned(),
        branch: v["ref"].as_str()?.strip_prefix("refs/heads/")?.to_owned(),
        sha: sha.to_owned(),
        actor: v["user_username"]
            .as_str()
            .or_else(|| v["user_name"].as_str())
            .unwrap_or("GitLab")
            .to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, Query},
        http::{HeaderMap, StatusCode as Code},
        routing::{get, post},
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "glpat-test-token";

    /// Requests the mock GitLab saw: (method and path, body).
    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    fn authed(h: &HeaderMap) -> bool {
        h.get("private-token").and_then(|v| v.to_str().ok()) == Some(TOKEN)
    }

    /// A small GitLab on this machine, with one group project.
    async fn mock() -> (Gitlab, Seen) {
        let seen: Seen = Arc::default();
        let s = seen.clone();
        let app = Router::new()
            .route(
                "/gitlab/api/v4/user",
                get(|h: HeaderMap| async move {
                    if !authed(&h) {
                        return (Code::UNAUTHORIZED, Json(json!({ "message": "401 Unauthorized" })));
                    }
                    (Code::OK, Json(json!({ "username": "group_42_bot_abc", "bot": true })))
                }),
            )
            .route(
                "/gitlab/api/v4/personal_access_tokens/self",
                get(|| async { Json(json!({ "scopes": ["api", "read_repository"], "expires_at": "2099-01-01", "active": true })) }),
            )
            .route("/gitlab/api/v4/version", get(|| async { Json(json!({ "version": "17.6.0" })) }))
            .route(
                "/gitlab/api/v4/groups",
                get(|| async { Json(json!([{ "full_path": "acme/web" }, { "full_path": "acme" }])) }),
            )
            .route(
                "/gitlab/api/v4/projects",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    assert_eq!(q["membership"], "true");
                    assert_eq!(q["min_access_level"], "40");
                    let all = json!([
                        { "id": 7, "path_with_namespace": "acme/web/shop", "name": "shop", "namespace": { "full_path": "acme/web" },
                          "visibility": "private", "default_branch": "main", "web_url": "https://gitlab.example.com/acme/web/shop", "last_activity_at": "2026-11-01T10:00:00Z" },
                        { "id": 8, "path_with_namespace": "acme/docs", "name": "docs", "namespace": { "full_path": "acme" },
                          "visibility": "public", "default_branch": "trunk", "web_url": "https://gitlab.example.com/acme/docs" },
                        { "id": 9, "name": "broken" }
                    ]);
                    let list: Vec<Value> = all.as_array().unwrap().iter().filter(|p| {
                        q.get("search").is_none_or(|s| p["path_with_namespace"].as_str().unwrap_or("").contains(s.as_str()))
                    }).cloned().collect();
                    Json(json!(list))
                }),
            )
            .route(
                "/gitlab/api/v4/projects/{id}/repository/branches",
                get(|Path(id): Path<String>| async move {
                    assert_eq!(id, "acme/web/shop", "the path is one segment");
                    Json(json!([
                        { "name": "main", "default": true, "protected": true, "commit": { "id": "a".repeat(40) } },
                        { "name": "feature/x", "commit": { "id": "b".repeat(40) } }
                    ]))
                }),
            )
            .route(
                "/gitlab/api/v4/projects/{id}/repository/branches/{branch}",
                get(|Path((_, branch)): Path<(String, String)>| async move {
                    if branch == "feature/x" {
                        (Code::OK, Json(json!({ "name": branch, "commit": { "id": "b".repeat(40) } })))
                    } else {
                        (Code::NOT_FOUND, Json(json!({ "message": "404 Branch Not Found" })))
                    }
                }),
            )
            .route(
                "/gitlab/api/v4/projects/{id}/repository/files/{file}",
                axum::routing::head(|Path((_, file)): Path<(String, String)>| async move {
                    if file == "apps/web/Dockerfile" { Code::OK } else { Code::NOT_FOUND }
                }),
            )
            .route(
                "/gitlab/api/v4/projects/{id}/statuses/{sha}",
                post({
                    let s = s.clone();
                    move |Path((id, sha)): Path<(String, String)>, Json(b): Json<Value>| async move {
                        let again = s.lock().unwrap().iter().any(|(k, v): &(String, Value)| k.ends_with(&sha) && v["state"] == b["state"]);
                        s.lock().unwrap().push((format!("POST {id} {sha}"), b.clone()));
                        if again {
                            return (Code::BAD_REQUEST, Json(json!({ "message": "Cannot transition status via :run from :running" })));
                        }
                        (Code::CREATED, Json(json!({ "id": 1 })))
                    }
                }),
            )
            .route(
                "/gitlab/api/v4/projects/{id}/deploy_tokens",
                post({
                    let s = s.clone();
                    move |Json(b): Json<Value>| async move {
                        s.lock().unwrap().push(("POST deploy_tokens".into(), b));
                        Json(json!({ "id": 31, "username": "gitlab+deploy-token-31", "token": "gldt-secret", "expires_at": "x" }))
                    }
                }),
            )
            .route(
                "/gitlab/api/v4/projects/{id}/hooks",
                post({
                    let s = s.clone();
                    move |Json(b): Json<Value>| async move {
                        s.lock().unwrap().push(("POST hooks".into(), b));
                        Json(json!({ "id": 55 }))
                    }
                })
                .get(|| async { Json(json!([{ "id": 54, "url": "https://pyo.example.com/api/git/gitc_x/webhook" }])) }),
            )
            .route(
                "/gitlab/api/v4/projects/{id}/hooks/{hook}",
                axum::routing::delete({
                    let s = s.clone();
                    move |Path((_, hook)): Path<(String, String)>| async move {
                        s.lock().unwrap().push((format!("DELETE hook {hook}"), Value::Null));
                        if hook == "54" { Code::NO_CONTENT } else { Code::NOT_FOUND }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = normalize(&format!("http://{addr}/gitlab/")).unwrap();
        (
            Gitlab::with_client(base, TOKEN, reqwest::Client::new()),
            seen,
        )
    }

    #[tokio::test]
    async fn lists_projects_and_branches() {
        let (g, _) = mock().await;
        let p = g.projects(None).await.unwrap();
        assert_eq!(p.len(), 2, "one without a path is skipped");
        assert_eq!(p[0].path, "acme/web/shop");
        assert!(p[0].private);
        assert_eq!(p[1].default_branch, "trunk");
        assert!(!p[1].private);
        let p = g.projects(Some("docs")).await.unwrap();
        assert_eq!(p.iter().map(|p| p.id).collect::<Vec<_>>(), vec![8]);

        let b = g.branches("acme/web/shop", None).await.unwrap();
        assert_eq!(b.len(), 2);
        assert!(b[0].default && b[0].protected);
        assert_eq!(b[1].name, "feature/x");
        assert_eq!(
            g.branch_head("acme/web/shop", "feature/x").await.unwrap(),
            "b".repeat(40)
        );
        let e = g.branch_head("acme/web/shop", "gone").await.unwrap_err();
        assert_eq!(e.status, Some(404));
        assert!(
            g.file_exists("acme/web/shop", "main", "apps/web/Dockerfile")
                .await
                .unwrap()
        );
        assert!(
            !g.file_exists("acme/web/shop", "main", "Dockerfile")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn checks_a_token() {
        let (g, _) = mock().await;
        let c = g.check().await.unwrap();
        assert_eq!(c.user, "group_42_bot_abc");
        assert_eq!(c.account, "acme", "the top group");
        assert_eq!(c.version, "17.6.0");
        assert_eq!(c.projects, 2);
        assert!(c.problems.is_empty(), "{:?}", c.problems);
        assert!(!c.fatal);

        let bad = Gitlab::with_client(g.base.clone(), "wrong", reqwest::Client::new());
        let e = bad.check().await.unwrap_err();
        assert_eq!(e.status, Some(401));
        assert!(e.message.contains("refused the token"), "{e}");
        assert!(!e.message.contains("wrong"));
    }

    #[test]
    fn says_what_a_token_misses() {
        let c = Check {
            scopes: vec!["read_repository".into()],
            projects: 3,
            ..Default::default()
        };
        assert_eq!(
            problems(&c, "2026-11-14"),
            vec!["The token needs the api scope. It has read_repository."]
        );
        let c = Check {
            scopes: vec!["api".into()],
            projects: 0,
            expires_at: Some("2026-11-20".into()),
            ..Default::default()
        };
        let p = problems(&c, "2026-11-14");
        assert_eq!(p.len(), 2);
        assert!(p[0].contains("Maintainer"));
        assert!(p[1].contains("expires on 2026-11-20"));
        let c = Check {
            scopes: vec!["api".into()],
            projects: 1,
            expires_at: Some("2027-11-20".into()),
            ..Default::default()
        };
        assert!(problems(&c, "2026-11-14").is_empty());
    }

    #[tokio::test]
    async fn sets_commit_statuses() {
        let (g, seen) = mock().await;
        let sha = "c".repeat(40);
        g.set_status(
            "acme/web/shop",
            &sha,
            status_state("running"),
            "Building…",
            "https://pyo.example.com/acme/deployments/dpl_1",
        )
        .await
        .unwrap();
        // The same state again is fine.
        g.set_status("acme/web/shop", &sha, "running", "Building…", "")
            .await
            .unwrap();
        g.set_status(
            "acme/web/shop",
            &sha,
            status_state("failure"),
            &"x".repeat(300),
            "",
        )
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, format!("POST acme/web/shop {sha}"));
        assert_eq!(seen[0].1["state"], "running");
        assert_eq!(seen[0].1["name"], "Pyo");
        assert_eq!(
            seen[0].1["target_url"],
            "https://pyo.example.com/acme/deployments/dpl_1"
        );
        assert!(seen[1].1.get("target_url").is_none());
        assert_eq!(seen[2].1["state"], "failed");
        assert_eq!(
            seen[2].1["description"].as_str().unwrap().chars().count(),
            140
        );
    }

    #[test]
    fn maps_deploy_states() {
        assert_eq!(status_state("pending"), "pending");
        assert_eq!(status_state("running"), "running");
        assert_eq!(status_state("success"), "success");
        assert_eq!(status_state("failure"), "failed");
        assert_eq!(status_state("error"), "canceled");
    }

    #[tokio::test]
    async fn makes_deploy_tokens_and_hooks() {
        let (g, seen) = mock().await;
        let t = g
            .create_deploy_token("acme/web/shop", "pyo-build-bj_1", "2026-11-14T12:00:00Z")
            .await
            .unwrap();
        assert_eq!(
            (t.id, t.username.as_str(), t.token.as_str()),
            (31, "gitlab+deploy-token-31", "gldt-secret")
        );
        assert_eq!(g.hooks("acme/web/shop").await.unwrap()[0].0, 54);
        assert_eq!(
            g.create_hook(
                "acme/web/shop",
                "https://pyo.example.com/api/git/gitc_x/webhook",
                "s3cret"
            )
            .await
            .unwrap(),
            55
        );
        g.delete_hook("acme/web/shop", 54).await.unwrap();
        g.delete_hook("acme/web/shop", 99).await.unwrap(); // gone already
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1["scopes"], json!(["read_repository"]));
        assert_eq!(seen[0].1["expires_at"], "2026-11-14T12:00:00Z");
        assert_eq!(seen[1].1["token"], "s3cret");
        assert_eq!(seen[1].1["push_events"], true);
        assert_eq!(seen[1].1["enable_ssl_verification"], true);
    }

    #[test]
    fn webhook_tokens() {
        assert!(token_ok("s3cret", Some("s3cret")));
        assert!(!token_ok("s3cret", Some("s3cre")));
        assert!(!token_ok("s3cret", Some("s3cret ")));
        assert!(!token_ok("s3cret", None));
        assert!(!token_ok("", Some("")));
    }

    #[test]
    fn push_events() {
        let body = json!({
            "object_kind": "push", "ref": "refs/heads/feature/x", "after": "d".repeat(40),
            "user_username": "ana", "project": { "path_with_namespace": "acme/web/shop" }
        });
        assert_eq!(
            push_of(Some("Push Hook"), &body),
            Some(Push {
                repo: "acme/web/shop".into(),
                branch: "feature/x".into(),
                sha: "d".repeat(40),
                actor: "ana".into()
            })
        );
        assert_eq!(push_of(Some("Tag Push Hook"), &body), None);
        assert_eq!(push_of(None, &body), None);
        let mut deleted = body.clone();
        deleted["after"] = json!("0".repeat(40));
        assert_eq!(push_of(Some("Push Hook"), &deleted), None);
        let mut tag = body;
        tag["ref"] = json!("refs/tags/v1");
        assert_eq!(push_of(Some("Push Hook"), &tag), None);
    }

    #[test]
    fn addresses() {
        assert_eq!(
            base_text(&normalize("https://gitlab.com/").unwrap()),
            "https://gitlab.com"
        );
        assert_eq!(
            base_text(&normalize(" https://git.example.com/gitlab/?x=1#y ").unwrap()),
            "https://git.example.com/gitlab"
        );
        assert!(normalize("https://user:pw@gitlab.com").is_err());
        assert!(normalize("ssh://git@gitlab.com").is_err());
    }

    /// The guarded client: GitLab addresses on this machine, link-local
    /// (cloud metadata) or the cluster's own networks are refused before
    /// any request, whatever the installation allows.
    #[tokio::test]
    async fn refuses_disallowed_addresses() {
        for addr in [
            "http://127.0.0.1:8080",
            "https://localhost",
            "http://169.254.169.254/latest",
            "https://10.43.0.1",
            "https://[::1]",
            "ftp://gitlab.example.com",
        ] {
            assert!(Gitlab::connect(addr, TOKEN).await.is_err(), "{addr}");
        }
        // Plain http to the internet would send the token in the clear.
        let e = Gitlab::connect("http://1.1.1.1", TOKEN)
            .await
            .err()
            .unwrap();
        assert!(e.message.contains("https://"), "{e}");
    }
}
