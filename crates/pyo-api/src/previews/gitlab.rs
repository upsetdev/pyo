//! Previews on GitLab, through the team's GitLab connection: merge request
//! events (the connection's webhook sends them), a note edited in place,
//! a commit status "Pyo · Preview", and an environment per preview
//! (`preview/web-pr-42`) with a deployment, which gives the merge request
//! its "View app" button.

use serde_json::Value;

use super::{Pull, PullRow, Remote, bot};
use crate::{AppState, git::gitlab::Gitlab};

/// A merge request from GitLab's API (`GET .../merge_requests`).
pub fn pull_of_api(v: &Value, connection_id: &str, repo: &str) -> Option<Pull> {
    let state = match v["state"].as_str()? {
        "opened" => "open",
        "merged" => "merged",
        _ => "closed",
    };
    Some(Pull {
        provider: "gitlab",
        installation_id: None,
        connection_id: Some(connection_id.to_owned()),
        repo: repo.to_owned(),
        number: v["iid"].as_i64()?,
        title: v["title"].as_str().unwrap_or("").to_owned(),
        url: v["web_url"].as_str().unwrap_or("").to_owned(),
        author: v["author"]["username"].as_str().unwrap_or("").to_owned(),
        head_ref: v["source_branch"].as_str().unwrap_or("").to_owned(),
        head_sha: v["sha"].as_str().filter(|s| s.len() == 40)?.to_owned(),
        base_ref: v["target_branch"].as_str().unwrap_or("").to_owned(),
        fork: v["source_project_id"] != v["target_project_id"],
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()).map(str::to_owned))
            .collect(),
        state,
    })
}

/// A merge request event (`X-Gitlab-Event: Merge Request Hook`), its token
/// already checked. None for anything else.
pub fn pull_of_hook(event: Option<&str>, v: &Value, conn: &crate::git::Connection) -> Option<Pull> {
    if event != Some("Merge Request Hook") || v["object_kind"].as_str() != Some("merge_request") {
        return None;
    }
    let a = &v["object_attributes"];
    let repo = conn
        .repo_of(v["project"]["web_url"].as_str().unwrap_or(""))
        .or_else(|| {
            v["project"]["path_with_namespace"]
                .as_str()
                .map(str::to_owned)
        })?;
    let state = match a["state"].as_str()? {
        "opened" => "open",
        "merged" => "merged",
        _ => "closed",
    };
    Some(Pull {
        provider: "gitlab",
        installation_id: None,
        connection_id: Some(conn.id.clone()),
        repo,
        number: a["iid"].as_i64()?,
        title: a["title"].as_str().unwrap_or("").to_owned(),
        url: a["url"].as_str().unwrap_or("").to_owned(),
        author: v["user"]["username"].as_str().unwrap_or("").to_owned(),
        head_ref: a["source_branch"].as_str().unwrap_or("").to_owned(),
        head_sha: a["last_commit"]["id"]
            .as_str()
            .filter(|s| s.len() == 40)?
            .to_owned(),
        base_ref: a["target_branch"].as_str().unwrap_or("").to_owned(),
        fork: a["source_project_id"] != a["target_project_id"],
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["title"].as_str().map(str::to_owned))
            .collect(),
        state,
    })
}

/// Whether the connection's webhook on the source's repository sends merge
/// request events (else Pyo asks GitLab every minute).
pub async fn hooked(db: &sqlx::PgPool, src: &Value) -> anyhow::Result<bool> {
    let Some(conn) = src["gitConnectionId"].as_str() else {
        return Ok(false);
    };
    let base: Option<String> =
        sqlx::query_scalar("SELECT base_url FROM pyo.git_connections WHERE id = $1")
            .bind(conn)
            .fetch_optional(db)
            .await?;
    let Some(repo) =
        base.and_then(|b| crate::git::repo_path(&b, src["repoUrl"].as_str().unwrap_or("")))
    else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.git_webhooks WHERE connection_id = $1 AND repo = $2 AND hook_id IS NOT NULL AND merge_requests)",
    )
    .bind(conn)
    .bind(repo.to_lowercase())
    .fetch_one(db)
    .await?)
}

/// The environment of a preview: "preview/web-pr-42" (one per merge
/// request, so each gets its own "View app").
pub fn environment(name: &str) -> String {
    format!("preview/{name}")
}

/// The bot on one merge request.
pub struct Forge {
    api: Gitlab,
    repo: String,
    number: i64,
    dashboard: String,
}

impl Forge {
    /// None when the connection is gone.
    pub async fn for_pull(s: &AppState, pull: &PullRow) -> anyhow::Result<Option<Forge>> {
        let Some(id) = &pull.connection_id else {
            return Ok(None);
        };
        let conn = match crate::git::load(&s.db, None, id).await {
            Ok(c) => c,
            Err(e) if e.status == axum::http::StatusCode::NOT_FOUND => return Ok(None),
            Err(e) => anyhow::bail!("{}", e.message),
        };
        let api = conn
            .api()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        Ok(Some(Forge {
            api,
            repo: pull.repo.clone(),
            number: pull.number,
            dashboard: crate::git::public_url(Some(&conn.public_url)),
        }))
    }

    #[cfg(test)]
    pub fn new(api: Gitlab, repo: &str, number: i64, dashboard: &str) -> Forge {
        Forge {
            api,
            repo: repo.to_owned(),
            number,
            dashboard: dashboard.to_owned(),
        }
    }

    pub fn dashboard(&self) -> &str {
        &self.dashboard
    }
}

impl super::Forge for Forge {
    async fn comment(&self, id: Option<i64>, body: &str) -> anyhow::Result<i64> {
        Ok(self
            .api
            .note(&self.repo, self.number, id.map(|i| i as u64), body)
            .await? as i64)
    }

    async fn check(
        &self,
        _id: Option<i64>,
        sha: &str,
        check: &bot::Check,
        details: Option<&str>,
    ) -> anyhow::Result<Option<i64>> {
        self.api
            .set_named_status(
                &self.repo,
                sha,
                bot::CHECK_NAME,
                check.gitlab_state(),
                &check.title,
                details.unwrap_or(""),
            )
            .await?;
        Ok(None)
    }

    async fn deployment(&self, d: &mut Remote, shown: bot::Shown) -> anyhow::Result<()> {
        let env = environment(&d.name);
        if matches!(shown, bot::Shown::Removed | bot::Shown::Removing) {
            if let Some(id) = d.environment_id.take() {
                self.api.remove_environment(&self.repo, id as u64).await?;
            }
            d.deployment_id = None;
            d.deployment_sha = None;
            return Ok(());
        }
        let Some(status) = shown.gitlab_deployment() else {
            return Ok(());
        };
        let env_id = self
            .api
            .environment(
                &self.repo,
                d.environment_id.map(|i| i as u64),
                &env,
                d.url.as_deref(),
            )
            .await?;
        d.environment_id = Some(env_id as i64);
        // A new head: a new deployment.
        let id = if d.deployment_sha.as_deref() == Some(d.sha.as_str()) {
            d.deployment_id.map(|i| i as u64)
        } else {
            None
        };
        let id = self
            .api
            .deployment(&self.repo, id, &env, &d.sha, &d.head_ref, status)
            .await?;
        d.deployment_id = Some(id as i64);
        d.deployment_sha = Some(d.sha.clone());
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
        http::StatusCode as Code,
        routing::{post, put},
    };
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn mr(state: &str, fork: bool) -> Value {
        json!({
            "iid": 7, "state": state, "title": "Add search", "web_url": "https://gitlab.com/acme/shop/-/merge_requests/7",
            "author": { "username": "ana" }, "source_branch": "search", "target_branch": "main",
            "sha": "c".repeat(40), "source_project_id": if fork { 99 } else { 1 }, "target_project_id": 1,
            "labels": ["preview"],
        })
    }

    #[test]
    fn merge_requests_from_the_api() {
        let p = pull_of_api(&mr("opened", false), "gitc_1", "acme/shop").unwrap();
        assert_eq!(
            (p.number, p.state, p.fork, p.provider),
            (7, "open", false, "gitlab")
        );
        assert_eq!(p.connection_id.as_deref(), Some("gitc_1"));
        assert_eq!(p.labels, vec!["preview"]);
        assert!(
            pull_of_api(&mr("opened", true), "gitc_1", "acme/shop")
                .unwrap()
                .fork
        );
        assert_eq!(
            pull_of_api(&mr("merged", false), "gitc_1", "acme/shop")
                .unwrap()
                .state,
            "merged"
        );
        assert_eq!(
            pull_of_api(&mr("closed", false), "gitc_1", "acme/shop")
                .unwrap()
                .state,
            "closed"
        );
    }

    /// Requests the fake GitLab saw.
    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    async fn fake() -> (Gitlab, Seen) {
        let seen: Seen = Seen::default();
        let app = Router::new()
            .route(
                "/api/v4/projects/{p}/merge_requests/{iid}/notes",
                post(
                    |State(s): State<Seen>,
                     Path((p, iid)): Path<(String, i64)>,
                     Json(b): Json<Value>| async move {
                        s.lock().unwrap().push((format!("POST note {p} {iid}"), b));
                        (Code::CREATED, Json(json!({ "id": 31 })))
                    },
                ),
            )
            .route(
                "/api/v4/projects/{p}/merge_requests/{iid}/notes/{id}",
                put(
                    |State(s): State<Seen>,
                     Path((_p, _iid, id)): Path<(String, i64, u64)>,
                     Json(b): Json<Value>| async move {
                        s.lock().unwrap().push((format!("PUT note {id}"), b));
                        if id == 404 {
                            return (Code::NOT_FOUND, Json(json!({ "message": "404 Not found" })));
                        }
                        (Code::OK, Json(json!({ "id": id })))
                    },
                ),
            )
            .route(
                "/api/v4/projects/{p}/statuses/{sha}",
                post(|State(s): State<Seen>, Json(b): Json<Value>| async move {
                    s.lock().unwrap().push(("POST status".into(), b));
                    (Code::CREATED, Json(json!({ "id": 1 })))
                }),
            )
            .route(
                "/api/v4/projects/{p}/environments",
                post(|State(s): State<Seen>, Json(b): Json<Value>| async move {
                    s.lock().unwrap().push(("POST environment".into(), b));
                    (Code::CREATED, Json(json!({ "id": 12 })))
                }),
            )
            .route(
                "/api/v4/projects/{p}/environments/{id}",
                put(|State(s): State<Seen>, Json(b): Json<Value>| async move {
                    s.lock().unwrap().push(("PUT environment".into(), b));
                    (Code::OK, Json(json!({ "id": 12 })))
                })
                .delete(|State(s): State<Seen>| async move {
                    s.lock()
                        .unwrap()
                        .push(("DELETE environment".into(), Value::Null));
                    Code::NO_CONTENT
                }),
            )
            .route(
                "/api/v4/projects/{p}/environments/{id}/stop",
                post(|State(s): State<Seen>| async move {
                    s.lock().unwrap().push(("POST stop".into(), Value::Null));
                    (Code::OK, Json(json!({})))
                }),
            )
            .route(
                "/api/v4/projects/{p}/deployments",
                post(|State(s): State<Seen>, Json(b): Json<Value>| async move {
                    s.lock().unwrap().push(("POST deployment".into(), b));
                    (Code::CREATED, Json(json!({ "id": 40 })))
                }),
            )
            .route(
                "/api/v4/projects/{p}/deployments/{id}",
                put(|State(s): State<Seen>, Json(b): Json<Value>| async move {
                    s.lock().unwrap().push(("PUT deployment".into(), b));
                    (Code::OK, Json(json!({ "id": 40 })))
                }),
            )
            .with_state(seen.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let base = reqwest::Url::parse(&format!("http://{addr}")).unwrap();
        (
            Gitlab::with_client(base, "glpat-x", reqwest::Client::new()),
            seen,
        )
    }

    fn kinds(s: &Seen) -> Vec<String> {
        s.lock().unwrap().iter().map(|(k, _)| k.clone()).collect()
    }

    #[tokio::test]
    async fn the_note_is_edited_in_place() {
        let (api, seen) = fake().await;
        let f = Forge::new(api, "acme/shop", 7, "");
        assert_eq!(f.comment(None, "one").await.unwrap(), 31);
        assert_eq!(f.comment(Some(31), "two").await.unwrap(), 31);
        // Deleted on GitLab: posted again.
        assert_eq!(f.comment(Some(404), "three").await.unwrap(), 31);
        assert_eq!(
            kinds(&seen),
            vec![
                "POST note acme/shop 7",
                "PUT note 31",
                "PUT note 404",
                "POST note acme/shop 7"
            ]
        );
    }

    #[tokio::test]
    async fn status_environment_and_deployment() {
        let (api, seen) = fake().await;
        let f = Forge::new(api, "acme/shop", 7, "");
        let failed = bot::Check {
            status: "completed",
            conclusion: Some("failure"),
            title: "Failed: web".into(),
            summary: String::new(),
        };
        f.check(None, &"c".repeat(40), &failed, Some("https://pyo/x"))
            .await
            .unwrap();
        let mut d = Remote {
            name: "web-pr-7".into(),
            service: "web".into(),
            sha: "c".repeat(40),
            head_ref: "search".into(),
            url: Some("https://web-pr-7.203.0.113.7.nip.io".into()),
            ..Default::default()
        };
        f.deployment(&mut d, bot::Shown::Waiting).await.unwrap();
        f.deployment(&mut d, bot::Shown::Building).await.unwrap();
        f.deployment(&mut d, bot::Shown::Ready).await.unwrap();
        f.deployment(&mut d, bot::Shown::Removed).await.unwrap();
        assert_eq!(
            kinds(&seen),
            vec![
                "POST status",
                "POST environment",
                "POST deployment",
                "PUT environment",
                "PUT deployment",
                "POST stop",
                "DELETE environment"
            ]
        );
        let s = seen.lock().unwrap().clone();
        assert_eq!(s[0].1["name"], "Pyo · Preview");
        assert_eq!(s[0].1["state"], "failed");
        assert_eq!(s[0].1["target_url"], "https://pyo/x");
        assert_eq!(s[1].1["name"], "preview/web-pr-7");
        assert_eq!(
            s[1].1["external_url"],
            "https://web-pr-7.203.0.113.7.nip.io"
        );
        assert_eq!(s[2].1["status"], "running");
        assert_eq!(s[2].1["environment"], "preview/web-pr-7");
        assert_eq!(s[2].1["ref"], "search");
        assert_eq!(s[4].1["status"], "success");
        assert_eq!(d.environment_id, None);
    }
}
