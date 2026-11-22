//! The Pyo API, as the CLI calls it: an API key in `Authorization`, JSON
//! in and out. Every change goes through `POST /api/v1/ops` like the
//! dashboard's.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

/// A change the API holds back until it's confirmed (a very large replica
/// change): send it again with `confirm: true` once the user says yes.
#[derive(Debug)]
pub struct NeedsConfirm(pub String);

impl std::fmt::Display for NeedsConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsConfirm {}

/// Pyo answered with an error: its status and message (what it shows).
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

/// Whether a request that failed with `e` is worth sending again: Pyo
/// wasn't reached, or it (or a proxy in front) had a passing problem.
pub fn retryable(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<Refused>() {
        Some(r) => r.status >= 500 || r.status == 408 || r.status == 429,
        None => true,
    }
}

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
        let text = res.text().await.unwrap_or_default();
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = error_message(status, &body, &text);
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("{msg}\nSign in again: pyo login");
            }
            // An operation this Pyo doesn't have: it's older than the CLI.
            if msg.starts_with("There's no operation named ") {
                bail!(
                    "This Pyo doesn't have that yet. It may be older than the CLI ({}): update it under Settings › Updates.",
                    crate::upgrade::VERSION
                );
            }
            if body["needsConfirm"] == true {
                return Err(NeedsConfirm(msg).into());
            }
            return Err(Refused {
                status: status.as_u16(),
                message: msg,
            }
            .into());
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

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.http.patch(format!("{}{path}", self.base)).json(body))
            .await
    }

    /// Raw bytes, e.g. a part of an upload.
    pub async fn put_bytes(&self, path: &str, body: Vec<u8>) -> Result<Value> {
        self.send(
            self.http
                .put(format!("{}{path}", self.base))
                .header("content-type", "application/octet-stream")
                .timeout(std::time::Duration::from_secs(300))
                .body(body),
        )
        .await
    }

    /// A part of an upload with its SHA-256 (hex), which Pyo checks.
    pub async fn put_part(&self, path: &str, body: Vec<u8>, sha256: &str) -> Result<Value> {
        self.send(
            self.http
                .put(format!("{}{path}", self.base))
                .header("content-type", "application/octet-stream")
                .header("x-pyo-sha256", sha256)
                .timeout(std::time::Duration::from_secs(300))
                .body(body),
        )
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

/// What a failed request says: the JSON `error`, else the body's text
/// (an older Pyo answers some bad requests in plain text), else the status.
fn error_message(status: reqwest::StatusCode, body: &Value, text: &str) -> String {
    let mut msg = match body["error"].as_str() {
        Some(e) => e.to_owned(),
        None => {
            let t = text.trim();
            if t.is_empty() || t.starts_with('<') {
                format!("Request failed ({status})")
            } else {
                t.chars().take(300).collect()
            }
        }
    };
    // Pyo answers a request it can't read with a 400. A 422 is an older
    // Pyo's, for something this CLI sends that it doesn't know.
    if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
        msg.push_str("\nThis Pyo may be older than the CLI. Update it or use an older CLI.");
    }
    msg
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

#[cfg(test)]
mod error_tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn only_passing_problems_are_retried() {
        let refused = |status| {
            anyhow::Error::from(Refused {
                status,
                message: "x".into(),
            })
        };
        assert!(retryable(&anyhow!("Can't reach Pyo")));
        assert!(retryable(&refused(502)) && retryable(&refused(524)));
        assert!(retryable(&refused(429)));
        assert!(!retryable(&refused(400)) && !retryable(&refused(409)));
        assert_eq!(refused(400).to_string(), "x");
        assert!(!retryable(
            &refused(410).context("The upload broke off. Try again.")
        ));
    }

    #[test]
    fn failed_requests_say_why() {
        let m = |s, body: &str| {
            error_message(s, &serde_json::from_str(body).unwrap_or(Value::Null), body)
        };
        assert_eq!(
            m(
                StatusCode::BAD_REQUEST,
                r#"{"error":"Add the team parameter."}"#
            ),
            "Add the team parameter."
        );
        assert_eq!(
            m(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Failed to deserialize the JSON body into the target type: missing field `template`\n"
            ),
            "Failed to deserialize the JSON body into the target type: missing field `template`\nThis Pyo may be older than the CLI. Update it or use an older CLI."
        );
        assert_eq!(
            m(StatusCode::BAD_GATEWAY, ""),
            "Request failed (502 Bad Gateway)"
        );
        assert_eq!(
            m(StatusCode::BAD_GATEWAY, "<html>"),
            "Request failed (502 Bad Gateway)"
        );
    }
}
