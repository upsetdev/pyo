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
        let connect = connect_src(crate::hosted(), addresses().sandboxes.as_deref());
        let csp = format!(
            "default-src 'self'; script-src 'nonce-{nonce}' 'strict-dynamic' 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; \
             font-src 'self' data:; connect-src {connect}; frame-src http: https:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self' https://github.com"
        );
        let mut res = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
        if let Ok(v) = HeaderValue::from_str(&csp) {
            res.headers_mut().insert(header::CONTENT_SECURITY_POLICY, v);
        }
        res
    }
}

/// Where the dashboard's own code may connect, beyond its own origin, for
/// sandboxes' terminals and files. On Pyo Cloud: the teams' sandbox
/// servers, on their tailnet names over HTTPS, which answer only on the
/// team's tailnet. Self-hosted: Pyo's own sandboxes name on the tailnet
/// (`sandboxes`, the [`Addresses`] one), which passes the calls on to the
/// servers (sandbox/pages.rs). Not their own plain HTTP addresses: a page
/// on HTTPS can't reach those, and servers join after the page loaded.
fn connect_src(hosted: bool, sandboxes: Option<&str>) -> String {
    if hosted {
        let port = crate::sandbox::outbound::TAILNET_PORT;
        return format!("'self' https://*.ts.net:{port} wss://*.ts.net:{port}");
    }
    // A name the header can't carry would drop the whole policy.
    let name = sandboxes.filter(|n| {
        !n.is_empty()
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    match name {
        Some(n) => format!("'self' https://{n} wss://{n}"),
        None => "'self'".into(),
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

/// This machine's tailnet name ("<host>.<tailnet>.ts.net"), once it's on
/// the tailnet (its host reports it, tailscale.rs `host_report`): for the
/// commands that name Pyo's tailnet IP (join.rs `friendly`).
pub fn tailnet_name() -> Option<String> {
    addresses().host_name
}

/// This machine's Tailscale IPv4 address, once it's on the tailnet.
pub fn tailnet_ip() -> Option<String> {
    addresses().host_ip
}

/// This API on the tailnet, for machines that join over it: its HTTPS name
/// when that's ready, else this machine's tailnet IP on :8000 (pyo-forward).
pub fn tailnet_api_url() -> Option<String> {
    let a = addresses();
    a.tailnet
        .map(|h| format!("https://{h}"))
        .or_else(|| a.host_ip.map(|ip| format!("http://{ip}:8000")))
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
    /// Sandboxes' web pages on the tailnet ("sandboxes.tail1234.ts.net"),
    /// which the API passes on to the sandbox servers (sandbox/pages.rs).
    pub sandboxes: Option<String>,
    /// The dashboard on the public IP, while it's open.
    pub public: Option<String>,
    /// This machine's Tailscale IPv4 address.
    pub host_ip: Option<String>,
    /// This machine's tailnet name ("<host>.<tailnet>.ts.net").
    pub host_name: Option<String>,
}

static ADDRESSES: std::sync::RwLock<Addresses> = std::sync::RwLock::new(Addresses {
    tailnet: None,
    registry: None,
    sandboxes: None,
    public: None,
    host_ip: None,
    host_name: None,
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
    [a.tailnet, a.public, a.registry, a.sandboxes]
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
    fn pyo_cloud_pages_reach_sandbox_servers_on_the_tailnet_only() {
        let cloud = "'self' https://*.ts.net:7443 wss://*.ts.net:7443";
        assert_eq!(super::connect_src(true, None), cloud);
        // Pyo Cloud's own name isn't how its pages reach sandboxes.
        assert_eq!(
            super::connect_src(true, Some("sandboxes.tail1234.ts.net")),
            cloud
        );
    }

    #[test]
    fn self_hosted_pages_reach_sandbox_servers_through_pyos_sandboxes_name() {
        // Tailscale set up, so sandbox servers can join: their terminals
        // and files go through Pyo's sandboxes name, and nowhere else.
        assert_eq!(
            super::connect_src(false, Some("sandboxes.tail1234.ts.net")),
            "'self' https://sandboxes.tail1234.ts.net wss://sandboxes.tail1234.ts.net"
        );
        // No Tailscale, so no sandbox servers: the page's own origin only.
        assert_eq!(super::connect_src(false, None), "'self'");
        // Never a name that would break or widen the policy.
        for bad in ["", "*", "a.ts.net; script-src *", "a.ts.net http:", "a/b"] {
            assert_eq!(super::connect_src(false, Some(bad)), "'self'", "{bad}");
        }
    }

    #[test]
    fn https_names_and_secure_cookies() {
        super::set_addresses(super::Addresses {
            tailnet: Some("pyo.tail1234.ts.net".into()),
            registry: Some("registry.tail1234.ts.net".into()),
            sandboxes: Some("sandboxes.tail1234.ts.net".into()),
            public: Some("pyo.203.0.113.7.nip.io".into()),
            host_ip: Some("100.64.0.7".into()),
            host_name: None,
        });
        assert_eq!(
            super::tailnet_api_url().as_deref(),
            Some("https://pyo.tail1234.ts.net")
        );
        assert!(super::is_https_host("PYO.tail1234.ts.net"));
        assert!(super::is_https_host("pyo.203.0.113.7.nip.io:443"));
        assert!(super::is_https_host("sandboxes.tail1234.ts.net"));
        assert!(!super::is_https_host("192.168.1.5:8000"));
        assert!(!super::is_https_host("localhost:8000"));
        assert_eq!(
            super::public_url().as_deref(),
            Some("https://pyo.tail1234.ts.net")
        );
        let mut hs = axum::http::HeaderMap::new();
        hs.append(
            header::SET_COOKIE,
            HeaderValue::from_static("a=1; Path=/; HttpOnly"),
        );
        hs.append(
            header::SET_COOKIE,
            HeaderValue::from_static("b=2; Path=/; Secure"),
        );
        super::secure_cookies(&mut hs);
        let got: Vec<&str> = hs
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            got,
            ["a=1; Path=/; HttpOnly; Secure", "b=2; Path=/; Secure"]
        );
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
