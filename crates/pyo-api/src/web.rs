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
pub async fn headers(force: axum::extract::State<bool>, req: Request, next: Next) -> Response {
    let assets = req.uri().path().starts_with("/assets/");
    // Over HTTPS (one of Pyo's HTTPS names, through the gateway): HSTS and
    // Secure cookies. Plain HTTP on this machine or a private network:
    // neither (the browser would drop a Secure cookie there).
    let https = *force
        || req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(is_https_host);
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
    if https {
        secure_cookies(hs);
        hs.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    res
}

/// This machine's tailnet name (`PYO_TAILNET_NAME`,
/// "<host>.<tailnet>.ts.net"), when it's on a tailnet with MagicDNS: for
/// the commands that name Pyo's tailnet IP (join.rs `friendly`).
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

/// Pyo's HTTPS names, as they're ready (tailscale.rs and the public
/// dashboard set them): on the tailnet ("pyo.tail1234.ts.net") and on the
/// public IP ("pyo.203.0.113.7.nip.io").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addresses {
    /// The dashboard on the tailnet.
    pub tailnet: Option<String>,
    /// The registry on the tailnet.
    pub registry: Option<String>,
    /// The dashboard on the public IP, while it's open.
    pub public: Option<String>,
}

static ADDRESSES: std::sync::RwLock<Addresses> = std::sync::RwLock::new(Addresses {
    tailnet: None,
    registry: None,
    public: None,
});

pub fn addresses() -> Addresses {
    ADDRESSES.read().map(|a| a.clone()).unwrap_or_default()
}

pub fn set_addresses(a: Addresses) {
    if let Ok(mut w) = ADDRESSES.write() {
        *w = a;
    }
}

/// Pyo's best HTTPS address: on the tailnet, else on the public IP. None
/// while it has neither (only on this machine and its network).
pub fn public_url() -> Option<String> {
    let a = addresses();
    a.tailnet.or(a.public).map(|h| format!("https://{h}"))
}

/// Every HTTPS address of the dashboard.
pub fn https_urls() -> Vec<String> {
    let a = addresses();
    [a.tailnet, a.public]
        .into_iter()
        .flatten()
        .map(|h| format!("https://{h}"))
        .collect()
}

/// Whether a request's Host is one of Pyo's HTTPS names (the gateway
/// terminates TLS for those alone).
pub fn is_https_host(host: &str) -> bool {
    let name = host
        .rsplit_once(':')
        .filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
        .map_or(host, |(h, _)| h)
        .to_ascii_lowercase();
    let a = addresses();
    [a.tailnet, a.public, a.registry]
        .into_iter()
        .flatten()
        .any(|h| h == name)
}

/// Cookies set over HTTPS get `Secure`.
fn secure_cookies(hs: &mut axum::http::HeaderMap) {
    let cookies: Vec<HeaderValue> = hs.get_all(header::SET_COOKIE).iter().cloned().collect();
    if cookies.is_empty() {
        return;
    }
    hs.remove(header::SET_COOKIE);
    for c in cookies {
        let v = match c.to_str() {
            Ok(t) if !t.to_ascii_lowercase().contains("; secure") => {
                HeaderValue::from_str(&format!("{t}; Secure")).unwrap_or(c)
            }
            _ => c,
        };
        hs.append(header::SET_COOKIE, v);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn https_names_and_secure_cookies() {
        super::set_addresses(super::Addresses {
            tailnet: Some("pyo.tail1234.ts.net".into()),
            registry: Some("registry.tail1234.ts.net".into()),
            public: Some("pyo.203.0.113.7.nip.io".into()),
        });
        assert!(super::is_https_host("PYO.tail1234.ts.net"));
        assert!(super::is_https_host("pyo.203.0.113.7.nip.io:443"));
        assert!(!super::is_https_host("192.168.1.5:8000"));
        assert!(!super::is_https_host("localhost:8000"));
        assert_eq!(super::public_url().as_deref(), Some("https://pyo.tail1234.ts.net"));
        let mut hs = axum::http::HeaderMap::new();
        hs.append(header::SET_COOKIE, HeaderValue::from_static("a=1; Path=/; HttpOnly"));
        hs.append(header::SET_COOKIE, HeaderValue::from_static("b=2; Path=/; Secure"));
        super::secure_cookies(&mut hs);
        let got: Vec<&str> = hs.get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(got, ["a=1; Path=/; HttpOnly; Secure", "b=2; Path=/; Secure"]);
        super::set_addresses(super::Addresses::default());
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
