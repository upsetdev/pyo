//! Sandboxes' web pages over HTTPS on the tailnet, like the desktop's
//! screen: `https://<sandboxes name>.<tailnet>.ts.net/v1/sandboxes/{id}/web/{port}/…`.
//!
//! The name is Pyo's own on the tailnet (tailscale.rs, "sandboxes" unless
//! someone else has it), and the tailnet Gateway sends it to this API like
//! the dashboard's. Here it's passed on to the sandbox's server, the same
//! path on its daemon (pyo-sandbox api.rs `web`), which passes it on to the
//! port in the sandbox. Browsers decode the desktop's video only on HTTPS
//! pages, and the daemon answers plain HTTP on the server's tailnet address,
//! so this is how a desktop opens in a browser.
//!
//! Pages there run whatever the sandbox serves, so they must never be on
//! the dashboard's origin: the name is a host of its own, this API answers
//! nothing else on it, and Pyo's session cookie (host-only) never reaches
//! it.
//!
//! The sandbox token (its `ports` scope, for that sandbox only) comes once
//! as `?pyo_token=`. It's checked, goes into an HttpOnly cookie for that
//! sandbox's path, and the browser is sent to the same address without it,
//! so it stays out of the address bar, history and logs. Later requests
//! carry the cookie (or `Authorization: Bearer`). Each is checked, then
//! goes to the daemon with the token as `Authorization: Bearer`, and
//! without the cookie. WebSocket upgrades pass through, for as long as
//! they stay open.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hyper_util::rt::TokioIo;
use pyo_proto::sandbox::Scope;
use pyo_proto::sandbox::token::{Expect, TOKEN_PREFIX, verify, verify_signature};

use crate::AppState;

/// The token's query parameter and cookie, as the daemon names them.
pub const TOKEN: &str = "pyo_token";
/// How long connecting to a sandbox server may take.
const CONNECT: Duration = Duration::from_secs(10);

/// Every request: on the sandboxes' name, a sandbox's page. Anything else
/// goes on to the API and the dashboard.
pub async fn gate(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let ours = crate::web::addresses().sandboxes;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(host_name);
    match (ours, host) {
        (Some(o), Some(h)) if o.eq_ignore_ascii_case(&h) => serve(&s, req).await,
        _ => next.run(req).await,
    }
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

/// `/v1/sandboxes/{id}/web/{port}` and what follows it.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    id: String,
    port: u16,
    /// The rest after `{port}/`. None when the path stops at the port.
    rest: Option<String>,
}

impl Target {
    fn parse(path: &str) -> Option<Self> {
        let p = path.strip_prefix("/v1/sandboxes/")?;
        let (id, p) = p.split_once('/')?;
        let p = p.strip_prefix("web/")?;
        let (port, rest) = match p.split_once('/') {
            Some((port, rest)) => (port, Some(rest.to_owned())),
            None => (p, None),
        };
        let ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if id.is_empty() || id.len() > 64 || !id.chars().all(ok) {
            return None;
        }
        Some(Target {
            id: id.to_owned(),
            port: port.parse().ok().filter(|p| *p > 0)?,
            rest,
        })
    }

    /// The path every page of this port is under, ending in a slash.
    fn prefix(&self) -> String {
        format!("/v1/sandboxes/{}/web/{}/", self.id, self.port)
    }

    /// The cookie's path: every port of the sandbox, as the daemon's.
    fn cookie_path(&self) -> String {
        format!("/v1/sandboxes/{}/web/", self.id)
    }
}

fn text(status: StatusCode, msg: &str) -> Response {
    let mut res = (status, format!("{msg}\n")).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// Takes the token out of the request: from `?pyo_token=` (then `true`),
/// the cookie or `Authorization: Bearer`. The query loses it and the
/// Cookie header loses it and Pyo's session cookie, whatever else happens.
fn take_token(req: &mut Request) -> Option<(String, bool)> {
    let mut found = None;
    let h = req.headers_mut();
    if let Some(t) = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_owned())
        .filter(|t| t.starts_with(&format!("{TOKEN_PREFIX}.")))
    {
        h.remove(header::AUTHORIZATION);
        found = Some((t, false));
    }
    let mut rest = Vec::new();
    for v in h.get_all(header::COOKIE) {
        for part in v.to_str().unwrap_or_default().split(';') {
            let part = part.trim();
            match part.split_once('=') {
                Some((TOKEN, val)) => {
                    found = found.or(Some((val.to_owned(), false)));
                }
                Some((crate::auth::SESSION_COOKIE, _)) => {}
                _ if !part.is_empty() => rest.push(part.to_owned()),
                _ => {}
            }
        }
    }
    h.remove(header::COOKIE);
    if let Some(v) = (!rest.is_empty())
        .then(|| HeaderValue::from_str(&rest.join("; ")).ok())
        .flatten()
    {
        h.insert(header::COOKIE, v);
    }
    if let Some(q) = req.uri().query() {
        let mut token = None;
        let kept: Vec<&str> = q
            .split('&')
            .filter(|kv| match kv.split_once('=') {
                Some((TOKEN, v)) => {
                    token = Some(v.to_owned());
                    false
                }
                _ => !kv.is_empty(),
            })
            .collect();
        if let Some(t) = token {
            let pq = if kept.is_empty() {
                req.uri().path().to_owned()
            } else {
                format!("{}?{}", req.uri().path(), kept.join("&"))
            };
            if let Ok(uri) = pq.parse::<Uri>() {
                *req.uri_mut() = uri;
            }
            found = Some((t, true));
        }
    }
    found
}

/// Why a request is refused: its status and a short message.
type Refusal = (StatusCode, &'static str);

/// Checks the token for this sandbox's ports and finds its server's
/// address.
async fn check(s: &AppState, id: &str, token: &str) -> Result<String, Refusal> {
    let refused = || {
        (
            StatusCode::UNAUTHORIZED,
            "The sandbox token isn't valid here. Open it again from Pyo.",
        )
    };
    let keys = crate::sandbox::keys::verifying_keys(&s.db, &s.sealer)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Pyo can't check tokens right now.",
            )
        })?;
    let claims = verify_signature(token, &keys).map_err(|_| refused())?;
    if claims.sbx != id {
        return Err(refused());
    }
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT x.server_id, sv.address FROM pyo.sandboxes x
           LEFT JOIN pyo.sandbox_servers sv ON sv.id = x.server_id WHERE x.id = $1",
    )
    .bind(id)
    .fetch_optional(&s.db)
    .await
    .map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Pyo can't find sandboxes right now.",
        )
    })?;
    let Some((Some(server), Some(address))) = row else {
        return Err((StatusCode::NOT_FOUND, "There is no such sandbox."));
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    verify(
        token,
        &keys,
        Expect {
            server: &server,
            sandbox: Some(id),
            scope: Scope::Ports,
            now,
        },
    )
    .map_err(|_| refused())?;
    Ok(address)
}

/// The cookie that keeps the token for this sandbox's pages.
fn token_cookie(t: &Target, token: &str) -> String {
    format!(
        "{TOKEN}={token}; Path={}; HttpOnly; Secure; SameSite=Lax",
        t.cookie_path()
    )
}

async fn serve(s: &AppState, mut req: Request) -> Response {
    let Some(target) = Target::parse(req.uri().path()) else {
        return text(StatusCode::NOT_FOUND, "Not found.");
    };
    let Some((token, from_query)) = take_token(&mut req) else {
        return text(
            StatusCode::UNAUTHORIZED,
            "This needs a sandbox token. Open it from Pyo.",
        );
    };
    let address = match check(s, &target.id, &token).await {
        Ok(a) => a,
        Err((status, msg)) => return text(status, msg),
    };
    if from_query || target.rest.is_none() {
        // Into a cookie, out of the address bar. Pages are under the
        // prefix, so it ends in a slash.
        let query = req
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let to = format!(
            "{}{}{query}",
            target.prefix(),
            target.rest.as_deref().unwrap_or_default()
        );
        let mut res = text(StatusCode::SEE_OTHER, "");
        let hs = res.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&to) {
            hs.insert(header::LOCATION, v);
        }
        if from_query && let Ok(v) = HeaderValue::from_str(&token_cookie(&target, &token)) {
            hs.insert(header::SET_COOKIE, v);
        }
        hs.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        return res;
    }
    proxy(&address, req, &token).await
}

/// The daemon's address for a server's tailnet address ("100.64.0.2:7070").
fn daemon_addr(address: &str) -> String {
    let base = crate::sandbox::daemon::base_url(address);
    base.strip_prefix("http://").unwrap_or(&base).to_owned()
}

/// Passes the request to the sandbox's server, a WebSocket too, and its
/// answer back.
async fn proxy(address: &str, mut req: Request, token: &str) -> Response {
    let bad_gateway = || {
        text(
            StatusCode::BAD_GATEWAY,
            "Pyo can't reach the sandbox server.",
        )
    };
    let addr = daemon_addr(address);
    let Ok(Ok(stream)) = tokio::time::timeout(CONNECT, tokio::net::TcpStream::connect(&addr)).await
    else {
        return bad_gateway();
    };
    let Ok((mut send, conn)) =
        hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(stream)).await
    else {
        return bad_gateway();
    };
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    prepare(req.headers_mut(), &addr, token);
    let client = req
        .headers()
        .contains_key(header::UPGRADE)
        .then(|| hyper::upgrade::on(&mut req));
    let Ok(mut res) = send.send_request(req).await else {
        return bad_gateway();
    };
    if res.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client) = client
    {
        let server = hyper::upgrade::on(&mut res);
        tokio::spawn(async move {
            if let (Ok(c), Ok(s)) = (client.await, server.await) {
                let _ =
                    tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(s)).await;
            }
        });
    }
    keep_cookies_here(res.headers_mut());
    res.map(Body::new)
}

/// The request as the daemon takes it: its own Host, and the token as a
/// bearer token.
fn prepare(h: &mut HeaderMap, addr: &str, token: &str) {
    if let Ok(v) = HeaderValue::from_str(addr) {
        h.insert(header::HOST, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        h.insert(header::AUTHORIZATION, v);
    }
}

/// A sandbox's page can set cookies for itself only: one with a Domain
/// would reach Pyo's other names on the tailnet, so it's dropped.
fn keep_cookies_here(h: &mut HeaderMap) {
    let kept: Vec<HeaderValue> = h
        .get_all(header::SET_COOKIE)
        .iter()
        .filter(|v| {
            !v.to_str().unwrap_or("domain=").split(';').any(|a| {
                a.trim()
                    .split_once('=')
                    .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("domain"))
            })
        })
        .cloned()
        .collect();
    h.remove(header::SET_COOKIE);
    for v in kept {
        h.append(header::SET_COOKIE, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            Target::parse("/v1/sandboxes/sbx_a1/web/6080/vnc.html"),
            Some(Target {
                id: "sbx_a1".into(),
                port: 6080,
                rest: Some("vnc.html".into())
            })
        );
        let bare = Target::parse("/v1/sandboxes/sbx_a1/web/6080").unwrap();
        assert_eq!(bare.rest, None);
        assert_eq!(bare.prefix(), "/v1/sandboxes/sbx_a1/web/6080/");
        assert_eq!(bare.cookie_path(), "/v1/sandboxes/sbx_a1/web/");
        for bad in [
            "/",
            "/api/v1/me",
            "/v1/sandboxes/sbx_a1/files",
            "/v1/sandboxes/sbx_a1/web/0/",
            "/v1/sandboxes/sbx_a1/web/http/",
            "/v1/sandboxes/../web/6080/",
            "/v1/sandboxes//web/6080/",
        ] {
            assert_eq!(Target::parse(bad), None, "{bad}");
        }
    }

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

    fn request(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut b = Request::builder().uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn the_token_from_the_query_once() {
        let mut r = request(
            "/v1/sandboxes/s/web/6080/vnc.html?autoconnect=1&pyo_token=pyosbx1.a.b&app=chromium",
            &[("cookie", "pyo_session=secret; other=1")],
        );
        assert_eq!(take_token(&mut r), Some(("pyosbx1.a.b".into(), true)));
        assert_eq!(
            r.uri().to_string(),
            "/v1/sandboxes/s/web/6080/vnc.html?autoconnect=1&app=chromium"
        );
        // Pyo's session never goes on.
        assert_eq!(r.headers()["cookie"], "other=1");
        let mut r = request("/v1/sandboxes/s/web/6080/?pyo_token=t", &[]);
        assert_eq!(take_token(&mut r), Some(("t".into(), true)));
        assert_eq!(r.uri().to_string(), "/v1/sandboxes/s/web/6080/");
    }

    #[test]
    fn the_token_from_the_cookie_or_a_header() {
        let mut r = request(
            "/v1/sandboxes/s/web/6080/ws",
            &[(
                "cookie",
                "pyo_desktop=x; pyo_token=pyosbx1.c.d; pyo_session=s",
            )],
        );
        assert_eq!(take_token(&mut r), Some(("pyosbx1.c.d".into(), false)));
        assert_eq!(r.headers()["cookie"], "pyo_desktop=x");
        let mut r = request(
            "/v1/sandboxes/s/web/6080/ws",
            &[
                ("authorization", "Bearer pyosbx1.e.f"),
                ("cookie", "pyo_token=x"),
            ],
        );
        assert_eq!(take_token(&mut r), Some(("pyosbx1.e.f".into(), false)));
        assert!(!r.headers().contains_key("authorization"));
        assert!(!r.headers().contains_key("cookie"));
        // A page's own bearer token isn't the sandbox's.
        let mut r = request("/x", &[("authorization", "Bearer app-token")]);
        assert_eq!(take_token(&mut r), None);
        assert_eq!(r.headers()["authorization"], "Bearer app-token");
    }

    #[test]
    fn to_the_daemon() {
        assert_eq!(daemon_addr("100.64.0.2"), "100.64.0.2:7070");
        assert_eq!(daemon_addr("127.0.0.1:17070"), "127.0.0.1:17070");
        let mut h = HeaderMap::new();
        h.insert(
            header::HOST,
            HeaderValue::from_static("sandboxes.tail1234.ts.net"),
        );
        prepare(&mut h, "100.64.0.2:7070", "pyosbx1.a.b");
        assert_eq!(h[header::HOST], "100.64.0.2:7070");
        assert_eq!(h[header::AUTHORIZATION], "Bearer pyosbx1.a.b");
        let t = Target::parse("/v1/sandboxes/s/web/6080/").unwrap();
        assert_eq!(
            token_cookie(&t, "pyosbx1.a.b"),
            "pyo_token=pyosbx1.a.b; Path=/v1/sandboxes/s/web/; HttpOnly; Secure; SameSite=Lax"
        );
    }

    #[test]
    fn cookies_for_other_names_are_dropped() {
        let mut h = HeaderMap::new();
        h.append(
            header::SET_COOKIE,
            HeaderValue::from_static("pyo_desktop=1; HttpOnly; SameSite=Lax"),
        );
        h.append(
            header::SET_COOKIE,
            HeaderValue::from_static("pyo_session=x; Domain=tail1234.ts.net; Path=/"),
        );
        h.append(
            header::SET_COOKIE,
            HeaderValue::from_static("a=b; domain =.ts.net"),
        );
        keep_cookies_here(&mut h);
        let left: Vec<_> = h.get_all(header::SET_COOKIE).iter().collect();
        assert_eq!(left, ["pyo_desktop=1; HttpOnly; SameSite=Lax"]);
    }
}
