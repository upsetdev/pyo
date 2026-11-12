//! HTTP client for the Pyo API's agent endpoints.

use std::time::Duration;

use anyhow::Context;
use pyo_proto::{
    AGENT_INSTANCE_HEADER, AGENT_VERSION_HEADER, CommandEnvelope, CommandResult,
    DeploymentProgress, Heartbeat, RegistryLogin, TlsCertificate,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

#[derive(Clone)]
pub struct PyoApi {
    base: String,
    http: reqwest::Client,
    token: String,
}

impl PyoApi {
    /// Whether Pyo runs on this cluster (we reach it in-cluster). For the
    /// agent's own choices only: Pyo goes by `host_proof`.
    pub fn same_cluster(&self) -> bool {
        pyo_proto::in_cluster_url(&self.base)
    }

    /// The proof that this is Pyo's own cluster (pyo-proto `host_proof`),
    /// when the host's bootstrap key is here to make it.
    pub async fn host_proof(&self, kube: &kube::Client) -> Option<String> {
        let secret = kube::Api::<k8s_openapi::api::core::v1::Secret>::namespaced(
            kube.clone(),
            pyo_proto::rbac::NS,
        )
        .get_opt(pyo_proto::BOOTSTRAP_SECRET)
        .await
        .ok()??;
        let key = secret
            .data?
            .get("key")
            .and_then(|k| String::from_utf8(k.0.clone()).ok())?;
        let key = key.trim();
        (!key.is_empty()).then(|| pyo_proto::host_proof(key, &pyo_proto::token_hash(&self.token)))
    }

    pub fn new(url: &str, token: &str) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("token has invalid characters")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(
            AGENT_VERSION_HEADER,
            HeaderValue::from_static(crate::AGENT_VERSION),
        );
        let instance = format!("{:x}{:x}", std::process::id(), pyo_proto::now_ms());
        headers.insert(AGENT_INSTANCE_HEADER, HeaderValue::from_str(&instance)?);
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            base: format!("{}/api/agent/v1", url.trim_end_matches('/')),
            http,
            token: token.to_owned(),
        })
    }

    async fn check(res: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("{status}: {body}")
    }

    /// Report the cluster; the reply carries what the API wants applied (node location labels).
    pub async fn heartbeat(&self, hb: &Heartbeat) -> anyhow::Result<pyo_proto::HeartbeatReply> {
        let res = self
            .http
            .post(format!("{}/heartbeat", self.base))
            .timeout(Duration::from_secs(20))
            .json(hb)
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await.unwrap_or_default())
    }

    pub async fn poll(&self, wait_secs: u64) -> anyhow::Result<Vec<CommandEnvelope>> {
        let res = self
            .http
            .get(format!("{}/commands?wait={wait_secs}", self.base))
            .timeout(Duration::from_secs(wait_secs + 15))
            .send()
            .await?;
        let items: Vec<serde_json::Value> = Self::check(res).await?.json().await?;
        let (commands, unreadable) = read_commands(items);
        for (id, why) in unreadable {
            tracing::warn!(%id, error = %why, "got a command this agent can't read; is it out of date?");
            let result = crate::failed(format!(
                "This cluster's agent is out of date and can't do this. Update it and try again. ({why})"
            ));
            if let Err(e) = self.command_result(&id, &result).await {
                tracing::warn!(%id, error = %e, "couldn't report an unreadable command");
            }
        }
        Ok(commands)
    }

    pub async fn command_result(&self, id: &str, r: &CommandResult) -> anyhow::Result<()> {
        let res = self
            .http
            .post(format!("{}/commands/{id}", self.base))
            .timeout(Duration::from_secs(20))
            .json(r)
            .send()
            .await?;
        Self::check(res).await?;
        Ok(())
    }

    /// The WebSocket a terminal session relays through.
    pub async fn terminal_socket(
        &self,
        session_id: &str,
    ) -> anyhow::Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = format!("{}/terminals/{session_id}", self.base).replacen("http", "ws", 1);
        let mut req = url.into_client_request()?;
        req.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))?,
        );
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(req),
        )
        .await
        .context("timed out")??;
        Ok(ws)
    }

    /// A private registry's login, for a service's pull secret.
    pub async fn registry_login(&self, id: &str) -> anyhow::Result<RegistryLogin> {
        let res = self
            .http
            .get(format!("{}/registry-logins/{id}", self.base))
            .timeout(Duration::from_secs(20))
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    /// An uploaded certificate and its key, for a domain using it.
    pub async fn certificate(&self, id: &str) -> anyhow::Result<TlsCertificate> {
        let res = self
            .http
            .get(format!("{}/certificates/{id}", self.base))
            .timeout(Duration::from_secs(20))
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    /// Retries briefly: a lost stage report leaves the dashboard stale.
    pub async fn progress(
        &self,
        deployment_id: &str,
        p: &DeploymentProgress,
    ) -> anyhow::Result<()> {
        let mut last = None;
        for attempt in 0..4 {
            let sent = self
                .http
                .post(format!(
                    "{}/deployments/{deployment_id}/progress",
                    self.base
                ))
                .timeout(Duration::from_secs(20))
                .json(p)
                .send()
                .await;
            match sent {
                Ok(res) => match Self::check(res).await {
                    Ok(_) => return Ok(()),
                    Err(e) => last = Some(e),
                },
                Err(e) => last = Some(e.into()),
            }
            tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("progress not sent")))
    }
}

/// Commands are read one at a time: an API newer than this agent may send
/// kinds it doesn't know, and those mustn't take the rest of the batch with
/// them, since the API has already marked the whole batch as sent.
fn read_commands(items: Vec<serde_json::Value>) -> (Vec<CommandEnvelope>, Vec<(String, String)>) {
    let mut commands = Vec::with_capacity(items.len());
    let mut unreadable = vec![];
    for item in items {
        let id = item["id"].as_str().unwrap_or("").to_owned();
        match serde_json::from_value(item) {
            Ok(c) => commands.push(c),
            Err(e) if !id.is_empty() => unreadable.push((id, e.to_string())),
            Err(e) => tracing::warn!(error = %e, "got a command with no id"),
        }
    }
    (commands, unreadable)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn an_unknown_command_doesnt_lose_the_rest_of_the_batch() {
        let service = json!({ "serviceId": "svc_1", "name": "web", "namespace": "shop", "workloadKind": "Deployment" });
        let (ok, bad) = super::read_commands(vec![
            json!({ "id": "c1", "command": { "kind": "stop", "serviceId": "svc_1", "name": "web", "namespace": "shop", "workloadKind": "Deployment" } }),
            json!({ "id": "c2", "command": { "kind": "from_the_future", "service": service } }),
            json!({ "id": "c3", "command": { "kind": "scale", "service": service, "replicas": 3 } }),
        ]);
        assert_eq!(
            ok.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["c1", "c3"]
        );
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].0, "c2");
    }
}
