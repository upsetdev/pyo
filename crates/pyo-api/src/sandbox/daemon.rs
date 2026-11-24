//! Pyo's calls to a sandbox server's daemon, over the tailnet
//! (`http://<tailnet IP>:7070`), each with a short-lived token Pyo signs.
//! A server that joined through a cluster is called through that cluster's
//! agent (pyo-proto's tailscale.rs `SandboxCall`): Pyo itself may be on no
//! tailnet (Pyo Cloud). A server that works over HTTPS (outbound/, Pyo
//! Cloud) isn't called at all: each call is queued as a signed command it
//! polls for, and its answer awaited (outbound/channel.rs `ask`). The
//! token isn't sent then: the signer's signature stands for Pyo.

use std::time::Duration;

use pyo_proto::sandbox::{
    DaemonCommand, DaemonError, DaemonResult, ErrorCode, ExecRequest, ExecResult, SandboxStatus,
    ServerReport, outbound,
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

/// The daemon's HTTPS URL on its tailnet name, for an outbound server.
pub fn tailnet_url(name: &str) -> String {
    format!(
        "https://{}:{}",
        name.trim_end_matches('.'),
        pyo_proto::sandbox::outbound::TAILNET_PORT
    )
}

pub struct Daemon {
    pub base: String,
    /// The server's tailnet address, as Pyo keeps it.
    address: String,
    /// The server's id.
    server_id: String,
}

/// How a call reaches a server.
enum Route {
    /// Straight to its daemon.
    Direct,
    /// Through this cluster's agent.
    Via(&'static crate::AppState, String),
    /// As a command it polls for.
    Outbound(&'static crate::AppState),
}

/// Pyo's state, for calls through a cluster's agent (set at start).
static STATE: std::sync::OnceLock<crate::AppState> = std::sync::OnceLock::new();

/// Calls to sandbox servers that joined through a cluster go through that
/// cluster's agent from now on, and calls to outbound servers through
/// their channel.
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
    /// The daemon of the server `server_id`, at its tailnet `address`.
    pub fn new(server_id: &str, address: &str) -> Daemon {
        Daemon {
            base: base_url(address),
            address: address.to_owned(),
            server_id: server_id.to_owned(),
        }
    }

    /// How calls reach this server: through a cluster's agent when it
    /// joined through one, as commands when it works over HTTPS, else
    /// straight to it.
    async fn route(&self) -> Route {
        let Some(s) = STATE.get() else {
            return Route::Direct;
        };
        let row: Option<(bool, Option<String>)> = sqlx::query_as(
            "SELECT outbound, via_cluster FROM pyo.sandbox_servers WHERE id = $1",
        )
        .bind(&self.server_id)
        .fetch_optional(&s.db)
        .await
        .ok()
        .flatten();
        match row {
            Some((true, _)) => Route::Outbound(s),
            Some((false, Some(c))) => Route::Via(s, c),
            _ => Route::Direct,
        }
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
        match self.route().await {
            Route::Via(s, cluster) => {
                return self.call_via(s, &cluster, method, path, token, body, timeout).await;
            }
            Route::Outbound(_) => {
                return Err(DaemonError::new(
                    ErrorCode::Internal,
                    "Pyo can't call a sandbox server that works over HTTPS.",
                ));
            }
            Route::Direct => {}
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

    /// A command to an outbound server, through its channel: its answer.
    async fn ask(
        &self,
        s: &crate::AppState,
        cmd: &DaemonCommand,
        wait: Duration,
    ) -> Result<DaemonResult, DaemonError> {
        match super::outbound::channel::ask(s, &self.server_id, cmd, wait).await? {
            DaemonResult::Error { error } => Err(error),
            r => Ok(r),
        }
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
        if let Route::Outbound(s) = self.route().await {
            return self.ask(s, cmd, TIMEOUT).await;
        }
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
            other => Err(something_else(other)),
        }
    }

    pub async fn report(&self, token: &str) -> Result<ServerReport, DaemonError> {
        if let Route::Outbound(s) = self.route().await {
            return match self.ask(s, &DaemonCommand::Report, TIMEOUT).await? {
                DaemonResult::Report { report } => Ok(*report),
                other => Err(something_else(other)),
            };
        }
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
        let ms = req.timeout_ms.unwrap_or(60_000);
        if let Route::Outbound(s) = self.route().await {
            // The server stops it by the channel's longest.
            let ms = if ms == 0 { outbound::EXEC_MAX_MS } else { ms.min(outbound::EXEC_MAX_MS) };
            let cmd = DaemonCommand::Exec {
                id: id.to_owned(),
                request: req.clone(),
            };
            return match self
                .ask(s, &cmd, Duration::from_millis(ms) + Duration::from_secs(30))
                .await?
            {
                DaemonResult::Exec { result } => Ok(result),
                other => Err(something_else(other)),
            };
        }
        let wait = Duration::from_millis(ms) + Duration::from_secs(10);
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
        if let Route::Outbound(s) = self.route().await {
            use base64::Engine as _;
            let cmd = DaemonCommand::ReadFile {
                id: id.to_owned(),
                path: path.to_owned(),
            };
            return match self.ask(s, &cmd, TIMEOUT).await {
                Ok(DaemonResult::File { data }) => base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| DaemonError::new(ErrorCode::Internal, "The sandbox server's answer didn't parse.")),
                Ok(other) => Err(something_else(other)),
                Err(e) => Err(over_the_channel(e)),
            };
        }
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
        if let Route::Outbound(s) = self.route().await {
            use base64::Engine as _;
            if data.len() > outbound::FILE_CAP {
                return Err(over_the_channel(DaemonError::new(ErrorCode::InvalidSpec, "")));
            }
            let cmd = DaemonCommand::WriteFile {
                id: id.to_owned(),
                path: path.to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(data),
            };
            return match self.ask(s, &cmd, TIMEOUT).await {
                Ok(DaemonResult::Written { bytes }) => Ok(bytes),
                Ok(other) => Err(something_else(other)),
                Err(e) => Err(over_the_channel(e)),
            };
        }
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

fn something_else(r: DaemonResult) -> DaemonError {
    DaemonError::new(
        ErrorCode::Internal,
        format!("The sandbox server answered with something else: {r:?}"),
    )
}

/// A file too big for the channel says where bigger ones go.
fn over_the_channel(e: DaemonError) -> DaemonError {
    // The daemon says "The file is bigger than 2 MiB."
    if e.code == ErrorCode::InvalidSpec && (e.message.is_empty() || e.message.starts_with("The file is bigger than")) {
        return DaemonError::new(
            ErrorCode::InvalidSpec,
            format!(
                "Through Pyo, files go up to {} MiB on this sandbox server. Move bigger ones from a device on your tailnet.",
                outbound::FILE_CAP >> 20
            ),
        );
    }
    e
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
        assert_eq!(tailnet_url("sbx-1.tail1234.ts.net."), "https://sbx-1.tail1234.ts.net:7443");
    }
}
