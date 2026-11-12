//! The Pyo API, as the CLI calls it: an API key in `Authorization`, JSON
//! in and out. Every change goes through `POST /api/v1/ops` like the
//! dashboard's.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

pub struct Api {
    pub base: String,
    token: String,
    http: reqwest::Client,
}

impl Api {
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("pyo-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let res = req
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("Can't reach Pyo at {}", self.base))?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = body["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Request failed ({status})"));
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("{msg}\nSign in again: pyo login");
            }
            bail!(msg);
        }
        Ok(body)
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body))
            .await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.send(self.http.delete(format!("{}{path}", self.base)))
            .await
    }

    /// One operation (see `Op` in crates/pyo-api/src/ops.rs).
    pub async fn op(&self, name: &str, mut args: Value) -> Result<Value> {
        args["op"] = json!(name);
        self.send(
            self.http
                .post(format!("{}/api/v1/ops", self.base))
                .json(&args),
        )
        .await
    }

    /// Everything the signed-in user can see, as the dashboard loads it.
    pub async fn state(&self) -> Result<State> {
        Ok(State(self.get("/api/v1/state").await?))
    }

    pub async fn me(&self) -> Result<Value> {
        self.get("/api/auth/me").await
    }
}

/// The dashboard's snapshot, with the lookups the CLI needs.
pub struct State(pub Value);

impl State {
    fn list(&self, key: &str) -> &[Value] {
        self.0[key].as_array().map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn services(&self) -> &[Value] {
        self.list("services")
    }

    pub fn service(&self, id: &str) -> Result<&Value> {
        self.services()
            .iter()
            .find(|a| a["id"] == json!(id))
            .ok_or_else(|| {
                anyhow!("Service {id} isn't in any of your teams. Link another one: pyo link")
            })
    }

    pub fn project(&self, id: &str) -> Option<&Value> {
        self.list("projects").iter().find(|p| p["id"] == json!(id))
    }

    pub fn teams(&self) -> &[Value] {
        self.list("teams")
    }

    pub fn team(&self, id: &str) -> Option<&Value> {
        self.list("teams").iter().find(|t| t["id"] == json!(id))
    }

    pub fn cluster(&self, id: &str) -> Option<&Value> {
        self.list("clusters").iter().find(|c| c["id"] == json!(id))
    }

    pub fn deployment(&self, id: &str) -> Option<&Value> {
        self.list("deployments")
            .iter()
            .find(|d| d["id"] == json!(id))
    }

    /// A service's deployments, newest first.
    pub fn deployments_of(&self, service_id: &str) -> Vec<&Value> {
        let mut d: Vec<&Value> = self
            .list("deployments")
            .iter()
            .filter(|d| d["serviceId"] == json!(service_id))
            .collect();
        d.sort_by_key(|d| std::cmp::Reverse(d["createdAt"].as_i64().unwrap_or(0)));
        d
    }

    /// "team/project/service", the way `pyo link` shows a service.
    pub fn path_of(&self, service: &Value) -> String {
        let p = self.project(service["projectId"].as_str().unwrap_or(""));
        let t = self.team(service["teamId"].as_str().unwrap_or(""));
        format!(
            "{}/{}/{}",
            t.and_then(|t| t["slug"].as_str()).unwrap_or("?"),
            p.and_then(|p| p["name"].as_str()).unwrap_or("?"),
            service["name"].as_str().unwrap_or("?")
        )
    }

    /// The service's page on the dashboard.
    pub fn service_url(&self, base: &str, service: &Value) -> String {
        let p = self.project(service["projectId"].as_str().unwrap_or(""));
        let t = self.team(service["teamId"].as_str().unwrap_or(""));
        format!(
            "{base}/{}/{}/{}",
            t.and_then(|t| t["slug"].as_str()).unwrap_or(""),
            p.and_then(|p| p["namespace"].as_str()).unwrap_or(""),
            service["name"].as_str().unwrap_or("")
        )
    }

    pub fn deployment_url(&self, base: &str, service: &Value, deployment_id: &str) -> String {
        let t = self.team(service["teamId"].as_str().unwrap_or(""));
        format!(
            "{base}/{}/deployments/{deployment_id}",
            t.and_then(|t| t["slug"].as_str()).unwrap_or("")
        )
    }
}

/// A client id the API accepts: `<prefix>_` and 10 lowercase letters or digits.
pub fn new_id(prefix: &str) -> String {
    const ABC: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 10];
    rand::fill(&mut bytes);
    let tail: String = bytes
        .iter()
        .map(|b| ABC[*b as usize % ABC.len()] as char)
        .collect();
    format!("{prefix}_{tail}")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
