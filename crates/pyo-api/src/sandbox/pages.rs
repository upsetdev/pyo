//! Pyo's own `sandboxes` name on the tailnet (tailscale.rs, "sandboxes"
//! unless someone else has it). Until 0.52.0 Pyo passed sandboxes' web
//! pages and the dashboard's terminal, files and commands on from there to
//! the daemon of a server that joined at its tailnet door. Pyo doesn't
//! connect to sandbox servers any more: a server answers browsers on its
//! own tailnet name over HTTPS (`https://<server>.<tailnet>.ts.net:7443`).
//!
//! Only links made before the update lead here, all to servers that joined
//! the old way, so the name answers [`super::NEEDS_UPDATE`] and nothing
//! else. Nothing of the API or the dashboard answers on it: pages there
//! once ran whatever a sandbox served.

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::AppState;

/// Every request: on the sandboxes' name, the update notice. Anything else
/// goes on to the API and the dashboard.
pub async fn gate(State(_): State<AppState>, req: Request, next: Next) -> Response {
    let ours = crate::web::addresses().sandboxes;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(host_name);
    match (ours, host) {
        (Some(o), Some(h)) if o.eq_ignore_ascii_case(&h) => notice(),
        _ => next.run(req).await,
    }
}

/// What the name answers: the update notice, readable from the
/// dashboard's origin.
fn notice() -> Response {
    let mut res = (StatusCode::CONFLICT, super::NEEDS_UPDATE).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res
}

/// A Host header's name, lower-case, without its port.
fn host_name(host: &str) -> String {
    let h = host.trim();
    h.rsplit_once(':')
        .filter(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .map_or(h, |(n, _)| n)
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(
            host_name("Sandboxes.tail1234.ts.net"),
            "sandboxes.tail1234.ts.net"
        );
        assert_eq!(
            host_name("sandboxes.tail1234.ts.net:443"),
            "sandboxes.tail1234.ts.net"
        );
        assert_eq!(
            host_name("sandboxes.tail1234.ts.net."),
            "sandboxes.tail1234.ts.net"
        );
    }

    #[tokio::test]
    async fn the_name_says_to_update_the_server() {
        let res = notice();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], super::super::NEEDS_UPDATE.as_bytes());
    }
}
