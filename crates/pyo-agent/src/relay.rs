//! Vector's servers' figures reach Pyo through this agent
//! (observability.rs); the gateway's access logs stay in the cluster, in
//! the traffic store (traffic/). Vector can't sign its
//! requests, and it gets no credential to Pyo: it posts here, inside the
//! cluster, with a token only this agent knows (made at its start), and the
//! agent sends each batch on as its own signed request.
//!
//! The listener is on the agent's pod address only, port 8089
//! (`PYO_AGENT_RELAY_PORT`): nothing on the servers' addresses.

use std::sync::OnceLock;

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};

use crate::Agent;

/// The largest batch passed on.
const MAX_BODY: usize = 8 << 20;

fn port() -> u16 {
    std::env::var("PYO_AGENT_RELAY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8089)
}

/// The token Vector sends: this process's own.
pub fn token() -> &'static str {
    static T: OnceLock<String> = OnceLock::new();
    T.get_or_init(|| format!("{}{}", pyo_proto::signing::new_nonce(), pyo_proto::signing::new_nonce()))
}

/// Where Vector posts: `http://<this pod's address>:<port>`. The pod's
/// address from Kubernetes (its name is the hostname).
pub async fn endpoint(client: &kube::Client) -> anyhow::Result<String> {
    static E: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    E.get_or_try_init(|| async {
        let name = std::env::var("HOSTNAME").unwrap_or_default();
        anyhow::ensure!(!name.is_empty(), "no HOSTNAME: can't tell this pod");
        let pod = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), pyo_proto::rbac::NS)
            .get(&name)
            .await?;
        let ip = pod
            .status
            .and_then(|s| s.pod_ip)
            .filter(|ip| !ip.is_empty())
            .ok_or_else(|| anyhow::anyhow!("this pod has no address yet"))?;
        let host = if ip.contains(':') { format!("[{ip}]") } else { ip };
        Ok(format!("http://{host}:{}", port()))
    })
    .await
    .cloned()
}

/// Whether a request carries this agent's relay token (constant time).
fn allowed(headers: &HeaderMap) -> bool {
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let want = token();
    got.len() == want.len() && got.bytes().zip(want.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

async fn forward(agent: &Agent, headers: &HeaderMap, path: &str, body: Bytes) -> impl IntoResponse + use<> {
    if !allowed(headers) {
        return (StatusCode::UNAUTHORIZED, String::new());
    }
    match agent.api.relay(path, body.to_vec()).await {
        Ok((status, text)) => (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), text),
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), path, "couldn't pass Vector's batch on");
            (StatusCode::BAD_GATEWAY, String::new())
        }
    }
}

/// Serves the relay for as long as the agent runs.
pub async fn serve(agent: Agent) {
    let app = Router::new()
        .route(
            "/host-metrics",
            post(|State(a): State<Agent>, h: HeaderMap, b: Bytes| async move { forward(&a, &h, "/host-metrics", b).await }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(agent);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port()));
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                tracing::info!(%addr, "relaying Vector's data to Pyo");
                if let Err(e) = axum::serve(l, app.clone()).await {
                    tracing::warn!(error = %e, "the relay stopped");
                }
            }
            Err(e) => tracing::warn!(%addr, error = %e, "the relay can't listen"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_agents_token_passes() {
        let mut h = HeaderMap::new();
        assert!(!allowed(&h));
        h.insert("authorization", format!("Bearer {}", token()).parse().unwrap());
        assert!(allowed(&h));
        h.insert("authorization", "Bearer nope".parse().unwrap());
        assert!(!allowed(&h));
    }
}
