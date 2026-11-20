//! Pyo's calls to a sandbox server's daemon, over the tailnet
//! (`http://<tailnet IP>:7070`), each with a short-lived token Pyo signs.
//! A server that joined through a cluster is called through that cluster's
//! agent (pyo-proto's tailscale.rs `SandboxCall`): Pyo itself may be on no
//! tailnet (Pyo Cloud).

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
    /// The server's tailnet address, as Pyo keeps it.
    address: String,
}

/// Pyo's state, for calls through a cluster's agent (set at start).
static STATE: std::sync::OnceLock<crate::AppState> = std::sync::OnceLock::new();

/// Calls to sandbox servers that joined through a cluster go through that
/// cluster's agent from now on.
pub fn init(s: crate::AppState) {
    let _ = STATE.set(s);
}

/// What goes in a request.
enum Body {
    None,
    Json(Vec<u8>),
    Bytes(Vec<u8>),
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
fn answer<T: serde::de::DeserializeOwned>(status: u16, body: &[u8]) -> Result<T, DaemonError> {
    if (200..300).contains(&status) {
        return serde_json::from_slice(body).map_err(|e| {
            DaemonError::new(
                ErrorCode::Internal,
                format!("The sandbox server's answer didn't parse: {e}"),
            )
        });
    }
    match serde_json::from_slice::<DaemonResult>(body) {
        Ok(DaemonResult::Error { error }) => Err(error),
        _ => Err(DaemonError::new(
            ErrorCode::Internal,
            format!(
                "The sandbox server answered {status}: {}",
                String::from_utf8_lossy(body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        )),
    }
}

/// `path` with its query, encoded.
fn with_query(path: &str, query: &[(&str, &str)]) -> String {
    if query.is_empty() {
        return path.to_owned();
    }
    let q = reqwest::Url::parse_with_params("http://q/", query)
        .ok()
        .and_then(|u| u.query().map(str::to_owned))
        .unwrap_or_default();
    format!("{path}?{q}")
}

impl Daemon {
    pub fn new(address: &str) -> Daemon {
        Daemon {
            base: base_url(address),
            address: address.to_owned(),
        }
    }

    /// The cluster whose agent reaches this server, when it joined
    /// through one.
    async fn via(&self) -> Option<(&'static crate::AppState, String)> {
        let s = STATE.get()?;
        let c: Option<String> = sqlx::query_scalar(
            "SELECT via_cluster FROM pyo.sandbox_servers WHERE address = $1 AND via_cluster IS NOT NULL LIMIT 1",
        )
        .bind(&self.address)
        .fetch_optional(&s.db)
        .await
        .ok()
        .flatten();
        c.map(|c| (s, c))
    }

    /// One call to the daemon: its status and body. Through the cluster's
    /// agent when the server joined through one, else straight to it.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Body,
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>), DaemonError> {
        if let Some((s, cluster)) = self.via().await {
            return self.call_via(s, &cluster, method, path, token, body, timeout).await;
        }
        let mut req = client()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .timeout(timeout);
        req = match body {
            Body::None => req,
            Body::Json(b) => req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(b),
            Body::Bytes(b) => req.body(b),
        };
        let r = req.send().await.map_err(unreachable)?;
        let status = r.status().as_u16();
        let body = r.bytes().await.map_err(unreachable)?;
        Ok((status, body.to_vec()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_via(
        &self,
        s: &crate::AppState,
        cluster: &str,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Body,
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>), DaemonError> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let (body, raw) = match body {
            Body::None => (None, false),
            Body::Json(b) => (Some(String::from_utf8_lossy(&b).into_owned()), false),
            Body::Bytes(b) => (Some(b64.encode(b)), true),
        };
        // Files come back as bytes.
        let raw = raw || (method == reqwest::Method::GET && path.contains("/files"));
        let call = pyo_proto::tailscale::SandboxCall {
            address: self.address.clone(),
            method: method.as_str().to_owned(),
            path: path.to_owned(),
            token: token.to_owned(),
            body,
            raw,
            content_type: None,
            timeout_secs: timeout.as_secs().max(1),
        };
        let (id, r) = crate::tailscale::ask(
            s,
            cluster,
            pyo_proto::Command::SandboxCall(Box::new(call)),
            timeout + Duration::from_secs(15),
        )
        .await
        .map_err(|e| {
            DaemonError::new(
                ErrorCode::Runtime,
                format!("Pyo couldn't reach the sandbox server through its cluster: {}", e.message),
            )
        })?;
        crate::tailscale::forget_command(&s.db, &id).await;
        let a: pyo_proto::tailscale::SandboxAnswer = r
            .data
            .and_then(|d| serde_json::from_value(d).ok())
            .ok_or_else(|| DaemonError::new(ErrorCode::Internal, "The cluster's agent gave no answer."))?;
        if a.status == 0 {
            return Err(DaemonError::new(ErrorCode::Runtime, a.body));
        }
        let bytes = if raw && (200..300).contains(&a.status) {
            b64.decode(&a.body).map_err(|_| {
                DaemonError::new(ErrorCode::Internal, "The sandbox server's answer didn't parse.")
            })?
        } else {
            a.body.into_bytes()
        };
        Ok((a.status, bytes))
    }

    pub async fn command(
        &self,
        token: &str,
        cmd: &DaemonCommand,
    ) -> Result<DaemonResult, DaemonError> {
        let body = serde_json::to_vec(cmd)
            .map_err(|e| DaemonError::new(ErrorCode::Internal, e.to_string()))?;
        let (status, b) = self
            .call(reqwest::Method::POST, "/v1/commands", token, Body::Json(body), TIMEOUT)
            .await?;
        match answer(status, &b)? {
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
        let (status, b) = self
            .call(reqwest::Method::GET, "/v1/report", token, Body::None, Duration::from_secs(10))
            .await?;
        answer(status, &b)
    }

    pub async fn exec(
        &self,
        token: &str,
        id: &str,
        req: &ExecRequest,
    ) -> Result<ExecResult, DaemonError> {
        let wait =
            Duration::from_millis(req.timeout_ms.unwrap_or(60_000)) + Duration::from_secs(10);
        let body = serde_json::to_vec(req)
            .map_err(|e| DaemonError::new(ErrorCode::Internal, e.to_string()))?;
        let (status, b) = self
            .call(
                reqwest::Method::POST,
                &format!("/v1/sandboxes/{id}/exec"),
                token,
                Body::Json(body),
                wait,
            )
            .await?;
        answer(status, &b)
    }

    pub async fn read_file(
        &self,
        token: &str,
        id: &str,
        path: &str,
    ) -> Result<Vec<u8>, DaemonError> {
        let (status, b) = self
            .call(
                reqwest::Method::GET,
                &with_query(&format!("/v1/sandboxes/{id}/files"), &[("path", path)]),
                token,
                Body::None,
                TIMEOUT,
            )
            .await?;
        if (200..300).contains(&status) {
            return Ok(b);
        }
        answer::<serde_json::Value>(status, &b).map(|_| vec![])
    }

    pub async fn write_file(
        &self,
        token: &str,
        id: &str,
        path: &str,
        data: Vec<u8>,
    ) -> Result<u64, DaemonError> {
        let (status, b) = self
            .call(
                reqwest::Method::PUT,
                &with_query(&format!("/v1/sandboxes/{id}/files"), &[("path", path)]),
                token,
                Body::Bytes(data),
                TIMEOUT,
            )
            .await?;
        let v: serde_json::Value = answer(status, &b)?;
        Ok(v["bytes"].as_u64().unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_encoded() {
        assert_eq!(
            with_query("/v1/sandboxes/s1/files", &[("path", "/a b&c")]),
            "/v1/sandboxes/s1/files?path=%2Fa+b%26c"
        );
        assert_eq!(with_query("/v1/report", &[]), "/v1/report");
    }

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
