//! How the dashboard's files and every response are served: compression,
//! caching and security headers.
//!
//! - `/assets/*` names carry a content hash, so browsers keep them for a year;
//!   the HTML shell is always revalidated so a release shows up on next load.
//! - The shell is served with a fresh nonce on every script tag, and a
//!   Content-Security-Policy that runs only those scripts and what they load
//!   (`'strict-dynamic'`): a script injected into a page can't run. Other
//!   sites can't frame the dashboard; it may frame any site (service previews).

use std::{path::Path, sync::Arc};

use axum::{
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;

/// The built `_shell.html`, kept in memory and stamped per request.
#[derive(Clone)]
pub struct Shell(Arc<Option<String>>);

impl Shell {
    pub fn load(path: &Path) -> Self {
        let html = std::fs::read_to_string(path).ok();
        if html.is_none() {
            tracing::info!(shell = %path.display(), "no dashboard shell to serve");
        }
        Self(Arc::new(html))
    }

    /// Any path the files don't have is a dashboard route: the shell.
    pub async fn serve(self) -> Response {
        let Some(html) = self.0.as_deref() else {
            return (StatusCode::NOT_FOUND, "The dashboard isn't built.").into_response();
        };
        let nonce = nonce();
        let body = stamp(html, &nonce);
        let csp = format!(
            "default-src 'self'; script-src 'nonce-{nonce}' 'strict-dynamic' 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; \
             font-src 'self' data:; connect-src 'self'; frame-src http: https:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self' https://github.com"
        );
        let mut res = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
        if let Ok(v) = HeaderValue::from_str(&csp) {
            res.headers_mut().insert(header::CONTENT_SECURITY_POLICY, v);
        }
        res
    }
}

fn nonce() -> String {
    let mut b = [0u8; 18];
    rand::fill(&mut b);
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// The nonce on every script and module preload of the shell.
fn stamp(html: &str, nonce: &str) -> String {
    html.replace("<script", &format!("<script nonce=\"{nonce}\""))
        .replace(
            "<link rel=\"modulepreload\"",
            &format!("<link rel=\"modulepreload\" nonce=\"{nonce}\""),
        )
}

/// Caching and security headers on every response.
pub async fn headers(https: axum::extract::State<bool>, req: Request, next: Next) -> Response {
    let assets = req.uri().path().starts_with("/assets/");
    let mut res = next.run(req).await;
    let html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/html"));
    let hs = res.headers_mut();
    // A missing asset gets the shell back: that mustn't be cached for a year.
    if assets && !html {
        hs.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    } else if html {
        hs.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    hs.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    hs.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    hs.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    hs.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    if *https {
        hs.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    res
}

/// The host's tailnet name (`PYO_TAILNET_NAME`, "<host>.<tailnet>.ts.net"),
/// set by its setup when the tailnet has MagicDNS and HTTPS certificates.
pub fn tailnet_name() -> Option<&'static str> {
    static N: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    N.get_or_init(|| {
        std::env::var("PYO_TAILNET_NAME")
            .ok()
            .map(|n| n.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|n| !n.is_empty() && n.contains('.'))
    })
    .as_deref()
}

/// Pyo's HTTPS address on its tailnet ("https://<name>:8443"); None while
/// it's only on its own machine (http://localhost:8000).
pub fn public_url() -> Option<&'static str> {
    static U: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    U.get_or_init(|| {
        tailnet_name().map(|n| format!("https://{n}:{}", pyo_proto::system::DASHBOARD_PORT))
    })
    .as_deref()
}

/// Where a request should go instead: the dashboard over HTTPS, for a
/// browser that reached plain HTTP at another address than this machine's
/// own (the tailnet's :8000). The API, installers, probes and anything
/// already at the HTTPS address stay as they are: agents and builders use
/// plain HTTP on the tailnet.
pub(crate) fn https_redirect(
    public: &str,
    method: &axum::http::Method,
    host: &str,
    path_and_query: &str,
) -> Option<String> {
    if !matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD) {
        return None;
    }
    let path = path_and_query.split('?').next().unwrap_or("");
    if ["/api/", "/install/", "/healthz", "/readyz"]
        .iter()
        .any(|p| path.starts_with(p))
    {
        return None;
    }
    let name = host
        .rsplit_once(':')
        .filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
        .map_or(host, |(h, _)| h);
    let public_host = public.trim_start_matches("https://");
    let local = matches!(name, "localhost" | "127.0.0.1" | "[::1]" | "")
        || name.ends_with(".svc")
        || name.ends_with(".svc.cluster.local");
    (!local && host != public_host).then(|| format!("{public}{path_and_query}"))
}

pub async fn redirect_to_https(req: Request, next: Next) -> Response {
    if let Some(public) = public_url() {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let pq = req.uri().path_and_query().map_or("/", |p| p.as_str());
        if let Some(to) = https_redirect(public, req.method(), host, pq) {
            return axum::response::Redirect::permanent(&to).into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_dashboard_moves_to_https_on_the_tailnet() {
        use axum::http::Method;
        let public = "https://server-1.tail1234.ts.net:8443";
        let r = |m: &Method, host: &str, pq: &str| super::https_redirect(public, m, host, pq);
        assert_eq!(
            r(&Method::GET, "100.64.0.7:8000", "/acme?x=1").as_deref(),
            Some("https://server-1.tail1234.ts.net:8443/acme?x=1")
        );
        assert_eq!(
            r(&Method::GET, "server-1.tail1234.ts.net:8000", "/").as_deref(),
            Some("https://server-1.tail1234.ts.net:8443/")
        );
        // This machine itself, already HTTPS, agents and builders: as they are.
        assert_eq!(r(&Method::GET, "localhost:8000", "/"), None);
        assert_eq!(r(&Method::GET, "127.0.0.1:8000", "/"), None);
        assert_eq!(r(&Method::GET, "server-1.tail1234.ts.net:8443", "/"), None);
        assert_eq!(
            r(&Method::GET, "100.64.0.7:8000", "/api/agent/v1/poll"),
            None
        );
        assert_eq!(
            r(
                &Method::GET,
                "100.64.0.7:8000",
                "/install/agent.yaml?token=x"
            ),
            None
        );
        assert_eq!(r(&Method::POST, "100.64.0.7:8000", "/"), None);
        assert_eq!(r(&Method::GET, "pyo.pyo-system.svc:8000", "/"), None);
    }

    use super::*;

    #[test]
    fn stamps_scripts_and_preloads() {
        let html = r#"<link rel="modulepreload" href="/a.js"/><script>x()</script><script type="module" src="/b.js"></script>"#;
        assert_eq!(
            stamp(html, "N"),
            r#"<link rel="modulepreload" nonce="N" href="/a.js"/><script nonce="N">x()</script><script nonce="N" type="module" src="/b.js"></script>"#
        );
        assert_ne!(nonce(), nonce());
    }
}
