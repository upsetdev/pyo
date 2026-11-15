//! Pyo's calls to a sandbox server's daemon, over the tailnet
//! (`http://<tailnet IP>:7070`), each with a short-lived token Pyo signs.

use std::time::Duration;

use pyo_proto::sandbox::{
    DaemonCommand, DaemonError, DaemonResult, ErrorCode, ExecRequest, ExecResult, SandboxStatus,
    ServerReport,
};

/// Calls that aren't exec wait this long at most.
const TIMEOUT: Duration = Duration::from_secs(20);

fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // The tailnet, never a proxy.
            .no_proxy()
            .build()
            .expect("an HTTP client")
    })
}

/// The daemon's base URL for a server's tailnet address. An address with a
/// port (a daemon on this machine, in tests) keeps it.
pub fn base_url(address: &str) -> String {
    let port = pyo_proto::sandbox::DAEMON_PORT;
    if let Ok(a) = address.parse::<std::net::SocketAddr>() {
        return format!("http://{a}");
    }
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v)) => format!("http://[{v}]:{port}"),
        _ => format!("http://{address}:{port}"),
    }
}

pub struct Daemon {
    pub base: String,
}

fn unreachable(e: reqwest::Error) -> DaemonError {
    DaemonError::new(
        ErrorCode::Runtime,
        if e.is_timeout() {
            "The sandbox server didn't answer in time.".to_owned()
        } else {
            format!("Pyo couldn't reach the sandbox server: {e}")
        },
    )
}

/// A daemon's answer: its result, or its error.
async fn answer<T: serde::de::DeserializeOwned>(r: reqwest::Response) -> Result<T, DaemonError> {
    let status = r.status();
    let body = r.bytes().await.map_err(unreachable)?;
    if status.is_success() {
        return serde_json::from_slice(&body).map_err(|e| {
            DaemonError::new(
                ErrorCode::Internal,
                format!("The sandbox server's answer didn't parse: {e}"),
            )
        });
    }
    match serde_json::from_slice::<DaemonResult>(&body) {
        Ok(DaemonResult::Error { error }) => Err(error),
        _ => Err(DaemonError::new(
            ErrorCode::Internal,
            format!(
                "The sandbox server answered {status}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        )),
    }
}

impl Daemon {
    pub fn new(address: &str) -> Daemon {
        Daemon {
            base: base_url(address),
        }
    }

    pub async fn command(
        &self,
        token: &str,
        cmd: &DaemonCommand,
    ) -> Result<DaemonResult, DaemonError> {
        let r = client()
            .post(format!("{}/v1/commands", self.base))
            .bearer_auth(token)
            .timeout(TIMEOUT)
            .json(cmd)
            .send()
            .await
            .map_err(unreachable)?;
        match answer(r).await? {
            DaemonResult::Error { error } => Err(error),
            r => Ok(r),
        }
    }

    /// A command whose answer is a sandbox.
    pub async fn sandbox(
        &self,
        token: &str,
        cmd: &DaemonCommand,
    ) -> Result<SandboxStatus, DaemonError> {
        match self.command(token, cmd).await? {
            DaemonResult::Sandbox { sandbox } => Ok(sandbox),
            other => Err(DaemonError::new(
                ErrorCode::Internal,
                format!("The sandbox server answered with something else: {other:?}"),
            )),
        }
    }

    pub async fn report(&self, token: &str) -> Result<ServerReport, DaemonError> {
        let r = client()
            .get(format!("{}/v1/report", self.base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(unreachable)?;
        answer(r).await
    }

    pub async fn exec(
        &self,
        token: &str,
        id: &str,
        req: &ExecRequest,
    ) -> Result<ExecResult, DaemonError> {
        let wait =
            Duration::from_millis(req.timeout_ms.unwrap_or(60_000)) + Duration::from_secs(10);
        let r = client()
            .post(format!("{}/v1/sandboxes/{id}/exec", self.base))
            .bearer_auth(token)
            .timeout(wait)
            .json(req)
            .send()
            .await
            .map_err(unreachable)?;
        answer(r).await
    }

    pub async fn read_file(
        &self,
        token: &str,
        id: &str,
        path: &str,
    ) -> Result<Vec<u8>, DaemonError> {
        let r = client()
            .get(format!("{}/v1/sandboxes/{id}/files", self.base))
            .query(&[("path", path)])
            .bearer_auth(token)
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(unreachable)?;
        if r.status().is_success() {
            return Ok(r.bytes().await.map_err(unreachable)?.to_vec());
        }
        answer::<serde_json::Value>(r).await.map(|_| vec![])
    }

    pub async fn write_file(
        &self,
        token: &str,
        id: &str,
        path: &str,
        data: Vec<u8>,
    ) -> Result<u64, DaemonError> {
        let r = client()
            .put(format!("{}/v1/sandboxes/{id}/files", self.base))
            .query(&[("path", path)])
            .bearer_auth(token)
            .timeout(TIMEOUT)
            .body(data)
            .send()
            .await
            .map_err(unreachable)?;
        let v: serde_json::Value = answer(r).await?;
        Ok(v["bytes"].as_u64().unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(base_url("100.64.0.7"), "http://100.64.0.7:7070");
        assert_eq!(
            base_url("fd7a:115c:a1e0::1"),
            "http://[fd7a:115c:a1e0::1]:7070"
        );
        assert_eq!(base_url("127.0.0.1:9999"), "http://127.0.0.1:9999");
    }
}
