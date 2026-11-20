//! Pyo's calls to a sandbox server's daemon, made from the cluster
//! (pyo-proto's tailscale.rs [`SandboxCall`]): the daemon answers plain
//! HTTP on the server's tailnet address, which the cluster's servers reach
//! and Pyo may not (Pyo Cloud runs outside every customer's tailnet).
//!
//! Only the daemon's own API: a tailnet address, its port, a path under
//! `/v1/`, and the token Pyo signed for the call. The daemon checks that
//! token itself, so this agent adds no access of its own.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use pyo_proto::CommandResult;
use pyo_proto::tailscale::{SandboxAnswer, SandboxCall, tailnet_ip};

use crate::{failed, ok};

/// The longest a call may take (an exec's own timeout, and some).
const MAX: Duration = Duration::from_secs(15 * 60);

fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // The tailnet, never a proxy.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client")
    })
}

/// The daemon's URL for a call, or why it isn't one Pyo makes.
pub fn url(c: &SandboxCall) -> Result<String, &'static str> {
    if !tailnet_ip(&c.address) {
        return Err("A sandbox server is called on its tailnet address only.");
    }
    if !c.path.starts_with("/v1/") || c.path.contains("..") || c.path.contains('#') {
        return Err("Only the sandbox server's own API is called.");
    }
    let port = pyo_proto::sandbox::DAEMON_PORT;
    Ok(match c.address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v)) => format!("http://[{v}]:{port}{}", c.path),
        _ => format!("http://{}:{port}{}", c.address, c.path),
    })
}

pub async fn run(c: &SandboxCall) -> CommandResult {
    let url = match url(c) {
        Ok(u) => u,
        Err(e) => return failed(e),
    };
    let method = match c.method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        _ => return failed("Not a method Pyo calls a sandbox server with."),
    };
    let mut req = client()
        .request(method, &url)
        .bearer_auth(&c.token)
        .timeout(Duration::from_secs(c.timeout_secs.max(1)).min(MAX));
    if let Some(ct) = &c.content_type {
        req = req.header(reqwest::header::CONTENT_TYPE, ct);
    }
    if let Some(b) = &c.body {
        req = if c.raw {
            match B64.decode(b) {
                Ok(bytes) => req.body(bytes),
                Err(_) => return failed("The body isn't base64."),
            }
        } else {
            req.header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(b.clone())
        };
    }
    let answer = match req.send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let content_type = r
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            match r.bytes().await {
                Ok(b) => SandboxAnswer {
                    status,
                    body: if c.raw && (200..300).contains(&status) {
                        B64.encode(&b)
                    } else {
                        String::from_utf8_lossy(&b).into_owned()
                    },
                    content_type,
                },
                Err(e) => unreachable(e),
            }
        }
        Err(e) => unreachable(e),
    };
    CommandResult {
        data: serde_json::to_value(answer).ok(),
        ..ok("Called the sandbox server.")
    }
}

/// Couldn't reach it: status 0, and why.
fn unreachable(e: reqwest::Error) -> SandboxAnswer {
    SandboxAnswer {
        status: 0,
        body: if e.is_timeout() {
            "The sandbox server didn't answer in time.".to_owned()
        } else {
            format!("Couldn't reach the sandbox server: {}", e.without_url())
        },
        content_type: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(address: &str, path: &str) -> SandboxCall {
        SandboxCall {
            address: address.into(),
            method: "GET".into(),
            path: path.into(),
            token: "t".into(),
            body: None,
            raw: false,
            content_type: None,
            timeout_secs: 5,
        }
    }

    #[test]
    fn only_the_daemon_on_the_tailnet() {
        assert_eq!(
            url(&call("100.64.0.7", "/v1/report")).unwrap(),
            "http://100.64.0.7:7070/v1/report"
        );
        assert_eq!(
            url(&call(
                "fd7a:115c:a1e0::1",
                "/v1/sandboxes/s1/files?path=%2Fa"
            ))
            .unwrap(),
            "http://[fd7a:115c:a1e0::1]:7070/v1/sandboxes/s1/files?path=%2Fa"
        );
        // Not the tailnet: the cluster's own network, the cloud's metadata.
        assert!(url(&call("10.43.0.1", "/v1/report")).is_err());
        assert!(url(&call("169.254.169.254", "/v1/report")).is_err());
        assert!(url(&call("example.com", "/v1/report")).is_err());
        // Not the daemon's API.
        assert!(url(&call("100.64.0.7", "/metrics")).is_err());
        assert!(url(&call("100.64.0.7", "/v1/../admin")).is_err());
        assert!(url(&call("100.64.0.7", "@evil/v1/")).is_err());
    }
}
