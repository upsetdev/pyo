//! The edge proxy: sandbox ports on the web at `<port>-<sandbox>.<domain>`,
//! like `3000-sbx-abc.sbx.example.com`.
//!
//! - Each request opens a `portConnect` stream to envd over vsock and
//!   speaks HTTP/1.1 to the guest port through it. So it needs no network
//!   inside the sandbox. WebSocket and other upgrades pass through.
//! - A port answers only once it's open (`openPort`, the `ports` scope).
//!   A public port answers anyone. A private one needs a token for this
//!   sandbox with the `ports` scope, in `Authorization: Bearer`,
//!   `X-Pyo-Token`, the `pyo_token` cookie, or once as `?pyo_token=`,
//!   which sets the cookie and redirects. The proxy strips the token
//!   before the request reaches the guest.
//! - HTTPS uses a configured certificate for `*.<domain>`. Getting it
//!   through DNS-01 comes with the Cloudflare DNS work. Plain HTTP
//!   redirects to HTTPS when both run, and serves ports when it runs
//!   alone, for testing.
//! - Only the edge (80 and 443) faces the internet on a sandbox server.
//!   The daemon API stays on the tailnet.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use pyo_proto::sandbox::token::TOKEN_PREFIX;
use pyo_proto::sandbox::wire::Scope;
use pyo_proto::sandbox::{SandboxState, parse_port_host};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::api::AppState;

type Body = BoxBody<Bytes, hyper::Error>;

/// The cookie and query parameter that carry a token for private ports.
pub const TOKEN_COOKIE: &str = "pyo_token";
pub const TOKEN_HEADER: &str = "x-pyo-token";
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// The `edge` part of the daemon's config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeConfig {
    /// Ports are served at `<port>-<sandbox>.<domain>`. Needs a wildcard
    /// DNS record pointing at the server.
    pub domain: String,
    /// The HTTPS listener, usually `0.0.0.0:443`. Needs `cert` and `key`.
    #[serde(default)]
    pub https: Option<SocketAddr>,
    /// PEM certificate chain for `*.<domain>`.
    #[serde(default)]
    pub cert: Option<std::path::PathBuf>,
    /// PEM private key of the certificate.
    #[serde(default)]
    pub key: Option<std::path::PathBuf>,
    /// The plain HTTP listener, usually `0.0.0.0:80`. It redirects to
    /// HTTPS when that runs too, and serves ports on its own otherwise.
    #[serde(default)]
    pub http: Option<SocketAddr>,
}

pub struct Edge {
    pub app: Arc<AppState>,
    pub cfg: EdgeConfig,
    tls: Option<tokio_rustls::TlsAcceptor>,
}

/// What the edge listens on once it runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bound {
    pub https: Option<SocketAddr>,
    pub http: Option<SocketAddr>,
}

impl Bound {
    /// The base URL for port URLs (see `CapabilityReport::port_url`).
    pub fn base_url(&self, domain: &str) -> Option<String> {
        let (scheme, addr, default) = match (self.https, self.http) {
            (Some(a), _) => ("https", a, 443),
            (None, Some(a)) => ("http", a, 80),
            (None, None) => return None,
        };
        let port = if addr.port() == default {
            String::new()
        } else {
            format!(":{}", addr.port())
        };
        Some(format!("{scheme}://{domain}{port}"))
    }
}

impl Edge {
    pub fn new(app: Arc<AppState>, cfg: EdgeConfig) -> anyhow::Result<Edge> {
        if cfg.domain.trim().is_empty() || cfg.domain.contains('/') {
            bail!("The edge domain {:?} isn't a domain.", cfg.domain);
        }
        if cfg.https.is_none() && cfg.http.is_none() {
            bail!("The edge has no listener. Set https, http or both.");
        }
        let tls = match cfg.https {
            Some(_) => {
                let (Some(cert), Some(key)) = (&cfg.cert, &cfg.key) else {
                    bail!("The edge's HTTPS listener needs cert and key.");
                };
                Some(tls_acceptor(cert, key)?)
            }
            None => None,
        };
        Ok(Edge { app, cfg, tls })
    }

    /// Binds the listeners and serves them in the background.
    pub async fn start(self: Arc<Self>) -> anyhow::Result<Bound> {
        let mut bound = Bound::default();
        if let Some(addr) = self.cfg.https {
            let l = TcpListener::bind(addr)
                .await
                .with_context(|| format!("edge listening on {addr}"))?;
            bound.https = Some(l.local_addr()?);
            tokio::spawn(self.clone().accept(l, true));
        }
        if let Some(addr) = self.cfg.http {
            let l = TcpListener::bind(addr)
                .await
                .with_context(|| format!("edge listening on {addr}"))?;
            bound.http = Some(l.local_addr()?);
            tokio::spawn(self.clone().accept(l, false));
        }
        Ok(bound)
    }

    async fn accept(self: Arc<Self>, l: TcpListener, tls: bool) {
        loop {
            let (tcp, peer) = match l.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "edge accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let me = self.clone();
            tokio::spawn(async move {
                if !tls {
                    return me.serve(TokioIo::new(tcp), peer, false).await;
                }
                let Some(acc) = me.tls.clone() else { return };
                if let Ok(Ok(s)) = tokio::time::timeout(HEADER_TIMEOUT, acc.accept(tcp)).await {
                    me.serve(TokioIo::new(s), peer, true).await;
                }
            });
        }
    }

    async fn serve<I>(self: Arc<Self>, io: I, peer: SocketAddr, tls: bool)
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let me = self.clone();
        let svc = hyper::service::service_fn(move |req| {
            let me = me.clone();
            async move { Ok::<_, Infallible>(me.handle(req, peer, tls).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_TIMEOUT)
            .serve_connection(io, svc)
            .with_upgrades()
            .await;
    }

    async fn handle(
        &self,
        mut req: Request<Incoming>,
        peer: SocketAddr,
        tls: bool,
    ) -> Response<Body> {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri().authority().map(|a| a.to_string()))
            .unwrap_or_default();
        if !tls && let Some(https) = self.cfg.https {
            let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(&host);
            let port = if https.port() == 443 {
                String::new()
            } else {
                format!(":{}", https.port())
            };
            let path = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/");
            return redirect(
                StatusCode::PERMANENT_REDIRECT,
                &format!("https://{name}{port}{path}"),
                None,
            );
        }
        let Some((port, id)) = parse_port_host(&host, &self.cfg.domain) else {
            return text(
                StatusCode::NOT_FOUND,
                "There is no sandbox port at this address.",
            );
        };
        let Ok(status) = self.app.mgr.status(&id) else {
            return text(
                StatusCode::NOT_FOUND,
                "There is no such sandbox on this server.",
            );
        };
        match status.state {
            SandboxState::Running => {}
            SandboxState::Paused => {
                return text(StatusCode::SERVICE_UNAVAILABLE, "The sandbox is paused.");
            }
            _ => {
                return text(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The sandbox isn't running.",
                );
            }
        }
        let Some(open) = status.ports.iter().find(|p| p.port == port) else {
            return text(
                StatusCode::NOT_FOUND,
                &format!("Port {port} of this sandbox isn't open."),
            );
        };
        let found = take_token(&mut req);
        if !open.public {
            let Some((token, from_query)) = found else {
                return text(
                    StatusCode::UNAUTHORIZED,
                    "This port is private. It needs a sandbox token.",
                );
            };
            // Only a token for this sandbox, never a server-wide one.
            if !matches!(self.app.authorize(Some(&token), Some(&id), Scope::Ports), Ok(c) if c.sbx == id)
            {
                return text(
                    StatusCode::UNAUTHORIZED,
                    "The sandbox token isn't valid for this port.",
                );
            }
            if from_query {
                // Into a cookie, so the token leaves the address bar and the
                // pages' own links work.
                let path = req
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/");
                let secure = if tls { "; Secure" } else { "" };
                let cookie =
                    format!("{TOKEN_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax{secure}");
                return redirect(StatusCode::SEE_OTHER, path, Some(&cookie));
            }
        }
        for (k, v) in forwarded(req.headers(), peer, tls, &host) {
            req.headers_mut().insert(k, v);
        }
        self.proxy(req, &id, port).await
    }

    async fn proxy(&self, mut req: Request<Incoming>, id: &str, port: u16) -> Response<Body> {
        let stream = match self.app.mgr.envd(id).await {
            Ok((_, conn)) => conn.port_stream(port).await.ok(),
            Err(_) => None,
        };
        let Some(stream) = stream else {
            return text(
                StatusCode::BAD_GATEWAY,
                &format!("Nothing answers on port {port} in the sandbox."),
            );
        };
        let closed = || {
            text(
                StatusCode::BAD_GATEWAY,
                "The sandbox port closed the connection.",
            )
        };
        let Ok((mut send, conn)) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
        else {
            return closed();
        };
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let client_upgrade = req
            .headers()
            .contains_key(header::UPGRADE)
            .then(|| hyper::upgrade::on(&mut req));
        let Ok(mut res) = send.send_request(req).await else {
            return closed();
        };
        if res.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some(client) = client_upgrade
        {
            let guest = hyper::upgrade::on(&mut res);
            tokio::spawn(async move {
                if let (Ok(c), Ok(g)) = (client.await, guest.await) {
                    let _ =
                        tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(g))
                            .await;
                }
            });
        }
        res.map(|b| b.boxed())
    }
}

/// Takes the sandbox token out of the request, so the guest never sees it.
/// Returns it and whether it came in the query.
fn take_token(req: &mut Request<Incoming>) -> Option<(String, bool)> {
    let mut found = None;
    let h = req.headers_mut();
    if let Some(t) = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string())
        .filter(|t| looks_like_token(t))
    {
        h.remove(header::AUTHORIZATION);
        found = Some((t, false));
    }
    if let Some(t) = h
        .remove(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok().map(str::to_string))
    {
        found = found.or(Some((t, false)));
    }
    if let Some((t, rest)) = cookie_token(h) {
        match rest {
            Some(v) => h.insert(header::COOKIE, v),
            None => h.remove(header::COOKIE),
        };
        found = found.or(Some((t, false)));
    }
    if let Some((t, uri)) = query_token(req.uri()) {
        *req.uri_mut() = uri;
        found = found.or(Some((t, true)));
    }
    found
}

/// A sandbox token, not the guest app's own bearer token.
fn looks_like_token(t: &str) -> bool {
    t.strip_prefix(TOKEN_PREFIX)
        .is_some_and(|rest| rest.starts_with('.'))
}

/// The token cookie, and the Cookie header without it (None when nothing
/// is left).
fn cookie_token(h: &HeaderMap) -> Option<(String, Option<HeaderValue>)> {
    let mut token = None;
    let mut rest = Vec::new();
    for v in h.get_all(header::COOKIE) {
        for part in v.to_str().ok()?.split(';') {
            let part = part.trim();
            match part.split_once('=') {
                Some((k, val)) if k == TOKEN_COOKIE => token = Some(val.to_string()),
                _ if !part.is_empty() => rest.push(part.to_string()),
                _ => {}
            }
        }
    }
    let token = token?;
    let rest = (!rest.is_empty())
        .then(|| HeaderValue::from_str(&rest.join("; ")).ok())
        .flatten();
    Some((token, rest))
}

/// The `pyo_token` query parameter and the URI without it.
fn query_token(uri: &hyper::Uri) -> Option<(String, hyper::Uri)> {
    let q = uri.query()?;
    let mut token = None;
    let rest: Vec<&str> = q
        .split('&')
        .filter(|kv| match kv.split_once('=') {
            Some((k, v)) if k == TOKEN_COOKIE => {
                token = Some(v.to_string());
                false
            }
            _ => true,
        })
        .collect();
    let token = token?;
    let path = uri.path();
    let pq = if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(pq.parse().ok()?);
    Some((token, hyper::Uri::from_parts(parts).ok()?))
}

fn forwarded(
    h: &HeaderMap,
    peer: SocketAddr,
    tls: bool,
    host: &str,
) -> Vec<(header::HeaderName, HeaderValue)> {
    let ip = peer.ip().to_string();
    let xff = match h.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {ip}"),
        None => ip,
    };
    let mut v = Vec::new();
    let mut put = |k: &'static str, val: &str| {
        if let Ok(val) = HeaderValue::from_str(val) {
            v.push((header::HeaderName::from_static(k), val));
        }
    };
    put("x-forwarded-for", &xff);
    put("x-forwarded-proto", if tls { "https" } else { "http" });
    put("x-forwarded-host", host);
    v
}

fn text(status: StatusCode, msg: &str) -> Response<Body> {
    let mut r = Response::new(
        Full::new(Bytes::from(format!("{msg}\n")))
            .map_err(|e| match e {})
            .boxed(),
    );
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn redirect(status: StatusCode, to: &str, cookie: Option<&str>) -> Response<Body> {
    let mut r = text(status, "Redirecting.");
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    if let Some(c) = cookie.and_then(|c| HeaderValue::from_str(c).ok()) {
        r.headers_mut().insert(header::SET_COOKIE, c);
    }
    r
}

fn tls_acceptor(cert: &Path, key: &Path) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {}", cert.display()))?;
    if certs.is_empty() {
        bail!("{} has no certificate.", cert.display());
    }
    let key =
        PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {}", key.display()))?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut cfg = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the edge certificate and key don't fit together")?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        let b = Bound {
            https: Some("0.0.0.0:443".parse().unwrap()),
            http: Some("0.0.0.0:80".parse().unwrap()),
        };
        assert_eq!(
            b.base_url("sbx.example.com").unwrap(),
            "https://sbx.example.com"
        );
        let h = Bound {
            https: None,
            http: Some("127.0.0.1:8081".parse().unwrap()),
        };
        assert_eq!(h.base_url("sbx.test").unwrap(), "http://sbx.test:8081");
        assert_eq!(Bound::default().base_url("x"), None);
    }

    #[test]
    fn tokens_come_out_of_cookies_and_queries() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; pyo_token=tok.sig; b=2"),
        );
        let (t, rest) = cookie_token(&h).unwrap();
        assert_eq!(t, "tok.sig");
        assert_eq!(rest.unwrap(), "a=1; b=2");
        h.insert(header::COOKIE, HeaderValue::from_static("pyo_token=x.y"));
        assert_eq!(cookie_token(&h).unwrap().1, None);
        h.insert(header::COOKIE, HeaderValue::from_static("a=1"));
        assert!(cookie_token(&h).is_none());

        let uri: hyper::Uri = "/app?x=1&pyo_token=tok.sig&y=2".parse().unwrap();
        let (t, u) = query_token(&uri).unwrap();
        assert_eq!(
            (t.as_str(), u.to_string().as_str()),
            ("tok.sig", "/app?x=1&y=2")
        );
        let (_, u) = query_token(&"/?pyo_token=a.b".parse().unwrap()).unwrap();
        assert_eq!(u.to_string(), "/");
        assert!(query_token(&"/?x=1".parse().unwrap()).is_none());
        assert!(looks_like_token("pyosbx1.abc.def"));
        assert!(!looks_like_token("eyJhbGciOi.x.y"));
    }

    #[test]
    fn forwarded_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let v = forwarded(&h, "5.6.7.8:9".parse().unwrap(), true, "3000-sbx-a.d");
        let get = |k: &str| v.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(get("x-forwarded-for"), "1.2.3.4, 5.6.7.8");
        assert_eq!(get("x-forwarded-proto"), "https");
        assert_eq!(get("x-forwarded-host"), "3000-sbx-a.d");
    }
}
