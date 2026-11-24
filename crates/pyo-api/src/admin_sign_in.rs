//! Where the installation's admins may sign in from (Settings › Security).
//!
//! An admin has powers over the whole installation, and on Pyo Cloud its
//! staff sign in on the same public page as everyone else. So the admin
//! can limit where admin accounts sign in: from the tailnet, and from
//! addresses or ranges (IPv4 and IPv6). Neither, the default: anywhere.
//! It's kept in `pyo.settings` as 'admin_sign_in'.
//!
//! From anywhere else, an admin account
//! - can't sign in, on any path: a password, a two-factor code, a passkey,
//!   a sign-in link, `pyo login`. The answer is the one a wrong password,
//!   code or link gets, so it tells nothing of the account or the limit.
//!   The account's audit log says so, and its email when Pyo sends email.
//! - has no browser session: its requests are unauthorized, as if signed
//!   out. The session stays, for when it's back where it may be. A stolen
//!   cookie works nowhere else.
//! - acts as a member only through API keys and assistants (OAuth): it
//!   keeps its teams but loses its admin powers. Keys never change the
//!   installation anyway (ops.rs `key_access_for`), and scripts and CI run
//!   from anywhere.
//!
//! Nobody else is ever affected.
//!
//! Where a request comes from is its visitor (ratelimit.rs `visitor_of`):
//! the TCP peer, unless that's a trusted proxy (Pyo's Gateway, cloudflared,
//! `PYO_TRUSTED_PROXIES`), whose headers alone count. It's on the tailnet
//! when that address is Tailscale's and it asked for a tailnet name
//! (`*.ts.net`): only the tailnet Gateway serves those, and it admits
//! Tailscale's addresses and the servers only. A public Gateway has no
//! route for them, so an address in Tailscale's range that isn't one (a
//! carrier's shared NAT) never counts. pyo-forward's :8000 shows up as a
//! server's own end of the pod network: it also admits local and private
//! peers, so it's never the tailnet.
//!
//! Locked out: a sign-in link from `pyo-reset-login` on the server turns
//! the limit off (recovery.rs). `PYO_ADMIN_SIGNIN_ANYWHERE=1` on Pyo's
//! deployment ignores it while it's set, for Pyo Cloud, which has no
//! server to run that on.

use std::{convert::Infallible, net::IpAddr};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{HeaderMap, header, request::Parts},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::{
    AppState,
    auth::User,
    error::{ApiError, ApiResult},
    ratelimit::Cidr,
};

/// The most addresses and ranges the limit takes.
pub const MAX_ADDRESSES: usize = 100;

/// The env var that ignores the limit while it's set ("1" or "true").
pub const ANYWHERE_ENV: &str = "PYO_ADMIN_SIGNIN_ANYWHERE";

/// Where admin accounts may sign in from. Neither: anywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limit {
    /// From the tailnet.
    #[serde(default)]
    pub tailnet: bool,
    /// From these addresses and ranges ("203.0.113.7", "2001:db8::/48").
    #[serde(default)]
    pub addresses: Vec<String>,
}

impl Limit {
    pub fn is_on(&self) -> bool {
        self.tailnet || !self.addresses.is_empty()
    }

    /// Whether an admin may sign in from `from`.
    pub fn allows(&self, from: &Source) -> bool {
        if !self.is_on() || (self.tailnet && from.tailnet) {
            return true;
        }
        from.ip.is_some_and(|ip| {
            self.addresses
                .iter()
                .filter_map(|a| Cidr::parse(a))
                .any(|c| c.contains(&ip))
        })
    }

    /// In words, for the activity: "the tailnet and 2 addresses".
    fn summary(&self) -> String {
        let n = self.addresses.len();
        let addresses = match n {
            0 => None,
            1 => Some(self.addresses[0].clone()),
            n => Some(format!("{n} addresses")),
        };
        match (self.tailnet, addresses) {
            (true, Some(a)) => format!("the tailnet and {a}"),
            (true, None) => "the tailnet".into(),
            (false, Some(a)) => a,
            (false, None) => "anywhere".into(),
        }
    }
}

/// Where a request comes from: its visitor's address, and whether it came
/// over the tailnet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    pub ip: Option<IpAddr>,
    pub tailnet: bool,
}

impl Source {
    /// From the TCP peer and the request's headers. `authority`: the URI's,
    /// for a request without a Host header (HTTP/2).
    pub fn of(peer: &str, headers: &HeaderMap, authority: Option<&str>) -> Self {
        Self::from_visitor(
            &crate::ratelimit::visitor_of(peer, headers),
            host_of(headers, authority),
        )
    }

    /// From a visitor's address (already worked out from trusted proxies
    /// only) and the host name it asked for.
    pub fn from_visitor(visitor: &str, host: &str) -> Self {
        let ip = visitor
            .trim()
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .ok()
            .map(|a| a.to_canonical());
        let name = host.split(':').next().unwrap_or("").trim_end_matches('.');
        let tailnet =
            ip.is_some_and(|a| tailscale_address(&a)) && pyo_proto::system::is_tailnet_host(name);
        Source { ip, tailnet }
    }

    pub fn from_parts(parts: &Parts) -> Self {
        let peer = parts
            .extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip().to_string())
            .unwrap_or_default();
        Self::of(
            &peer,
            &parts.headers,
            parts.uri.authority().map(|a| a.as_str()),
        )
    }

    /// The address in words.
    pub fn shown(&self) -> String {
        self.ip
            .map(|a| a.to_string())
            .unwrap_or_else(|| "an unknown address".into())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Source {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Source::from_parts(parts))
    }
}

/// The host name a request asked for: its Host header, else (HTTP/2) its
/// URI's authority.
pub fn host_of<'a>(headers: &'a HeaderMap, authority: Option<&'a str>) -> &'a str {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or(authority)
        .unwrap_or("")
}

/// One of Tailscale's addresses (100.64.0.0/10, fd7a:115c:a1e0::/48).
fn tailscale_address(ip: &IpAddr) -> bool {
    pyo_proto::system::TAILNET_CIDRS
        .iter()
        .filter_map(|c| Cidr::parse(c))
        .any(|c| c.contains(ip))
}

/// Whether `PYO_ADMIN_SIGNIN_ANYWHERE` is set: the limit is ignored.
pub fn anywhere() -> bool {
    static A: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *A.get_or_init(|| {
        let on = matches!(
            std::env::var(ANYWHERE_ENV).as_deref().map(str::trim),
            Ok("1" | "true")
        );
        if on {
            tracing::warn!("{ANYWHERE_ENV} is set: admins may sign in from anywhere");
        }
        on
    })
}

/// The installation's limit, as saved.
pub async fn load(db: impl sqlx::PgExecutor<'_>) -> sqlx::Result<Limit> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'admin_sign_in'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

/// Whether an admin account may act from `from`: what the limit allows,
/// or anywhere while `PYO_ADMIN_SIGNIN_ANYWHERE` is set.
pub async fn admits(db: impl sqlx::PgExecutor<'_>, from: &Source) -> sqlx::Result<bool> {
    if anywhere() {
        return Ok(true);
    }
    Ok(load(db).await?.allows(from))
}

/// A limit to save: each address or range checked and written plainly
/// (its network address, no /32 or /128), duplicates dropped.
pub fn checked(tailnet: bool, addresses: &[String]) -> ApiResult<Limit> {
    let mut out: Vec<String> = vec![];
    for a in addresses.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
        let well_formed = match a.split_once('/') {
            Some((_, len)) => !len.is_empty() && len.bytes().all(|b| b.is_ascii_digit()),
            None => true,
        };
        let cidr = Cidr::parse(a).filter(|_| well_formed).ok_or_else(|| {
            ApiError::bad_request(format!("{a} isn't an IP address or range."))
        })?;
        let plain = cidr.to_string();
        if !out.contains(&plain) {
            out.push(plain);
        }
    }
    if out.len() > MAX_ADDRESSES {
        return Err(ApiError::bad_request(format!(
            "Add up to {MAX_ADDRESSES} addresses."
        )));
    }
    Ok(Limit {
        tailnet,
        addresses: out,
    })
}

/// A new limit must keep letting in the request that saves it, so its
/// admin can't lock themselves out.
pub fn keeps(limit: &Limit, here: &Source) -> ApiResult<()> {
    if limit.allows(here) {
        return Ok(());
    }
    Err(ApiError::bad_request(match here.ip {
        Some(ip) => format!("This would lock you out. Add your address {ip} first."),
        None => "This would lock you out. Pyo can't tell your address.".to_owned(),
    }))
}

/// Saves the limit (off when it allows anywhere). Returns what it was.
pub async fn save(conn: &mut PgConnection, limit: &Limit) -> sqlx::Result<Limit> {
    let before = load(&mut *conn).await?;
    if limit.is_on() {
        sqlx::query(
            "INSERT INTO pyo.settings (key, value) VALUES ('admin_sign_in', $1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
        )
        .bind(json!(limit))
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query("DELETE FROM pyo.settings WHERE key = 'admin_sign_in'")
            .execute(&mut *conn)
            .await?;
    }
    Ok(before)
}

/// What the activity says of a change.
pub fn change_title(limit: &Limit) -> String {
    if limit.is_on() {
        format!("Limited admin sign-in to {}", limit.summary())
    } else {
        "Let admins sign in from anywhere".into()
    }
}

/// Turns the limit off (a sign-in link made on the server). Whether it
/// was on.
pub async fn turn_off(conn: &mut PgConnection) -> sqlx::Result<bool> {
    Ok(save(conn, &Limit::default()).await?.is_on())
}

/// Whether `user` may sign in from `from` (`how`: "with a password").
/// Everyone may, but the installation's admins only from where the limit
/// allows. A refusal goes to the account's audit log, and its email.
/// `user.is_admin` is the account's own (`pyo.users.is_admin`).
pub async fn sign_in_allowed(
    state: &AppState,
    user: &User,
    from: &Source,
    how: &str,
) -> ApiResult<bool> {
    if !user.is_admin || admits(&state.db, from).await? {
        return Ok(true);
    }
    refused(state, user, from, how).await;
    Ok(false)
}

/// A refused admin sign-in, in the audit log and the account's email.
pub async fn refused(state: &AppState, user: &User, from: &Source, how: &str) {
    tracing::warn!(user = %user.id, ip = %from.shown(), how, "admin sign-in refused: not from an allowed address");
    crate::audit::account_event(
        &state.db,
        &user.id,
        &crate::audit::Who::person(user),
        crate::audit::Entry {
            action: "auth.sign_in_refused",
            title: &format!("Sign-in {how} refused outside the allowed addresses"),
            target: json!({}),
            failed: true,
        },
    )
    .await;
    let (s, to, ip) = (state.clone(), user.email.clone(), from.shown());
    tokio::spawn(async move {
        crate::twofactor::mail_user(&s, &to, "/security", |link| {
            crate::mail::sign_in_refused(&ip, link)
        })
        .await;
    });
}

/// For the dashboard (its admin): the limit, where this request comes
/// from, whether the tailnet can be chosen, and whether the limit is
/// ignored (`PYO_ADMIN_SIGNIN_ANYWHERE`).
pub async fn snapshot(db: &sqlx::PgPool, here: &Source) -> sqlx::Result<Value> {
    let limit = load(db).await?;
    Ok(json!({
        "tailnet": limit.tailnet,
        "addresses": limit.addresses,
        "here": here.ip.map(|a| a.to_string()),
        "hereTailnet": here.tailnet,
        "tailnetAvailable": crate::web::addresses().tailnet.is_some() || here.tailnet,
        "anywhere": anywhere(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn src(ip: &str, tailnet: bool) -> Source {
        Source {
            ip: Some(ip.parse().unwrap()),
            tailnet,
        }
    }

    fn limit(tailnet: bool, addresses: &[&str]) -> Limit {
        checked(
            tailnet,
            &addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn off_by_default_allows_anywhere() {
        let l = Limit::default();
        assert!(!l.is_on());
        assert!(l.allows(&src("203.0.113.7", false)));
        assert!(l.allows(&Source::default()));
        let saved: Limit = serde_json::from_value(json!({})).unwrap();
        assert_eq!(saved, l);
    }

    #[test]
    fn addresses_and_ranges_v4_and_v6() {
        let l = limit(false, &["203.0.113.7", "198.51.100.0/24", "2001:db8:42::/48"]);
        assert!(l.allows(&src("203.0.113.7", false)));
        assert!(!l.allows(&src("203.0.113.8", false)));
        assert!(l.allows(&src("198.51.100.250", false)));
        assert!(!l.allows(&src("198.51.101.1", false)));
        assert!(l.allows(&src("2001:db8:42:1::9", false)));
        assert!(!l.allows(&src("2001:db8:43::9", false)));
        // IPv4 written as IPv6 is the same address.
        let mapped = Source {
            ip: Some("::ffff:203.0.113.7".parse::<IpAddr>().unwrap().to_canonical()),
            tailnet: false,
        };
        assert!(l.allows(&mapped));
        // No address: nothing to match.
        assert!(!l.allows(&Source::default()));
        // The tailnet isn't on the list.
        assert!(!l.allows(&src("100.64.0.103", true)));
    }

    #[test]
    fn tailnet_only() {
        let l = limit(true, &[]);
        assert!(l.is_on());
        assert!(l.allows(&src("100.64.0.103", true)));
        // A Tailscale address that didn't come over the tailnet.
        assert!(!l.allows(&src("100.64.0.103", false)));
        assert!(!l.allows(&src("203.0.113.7", false)));
        // Both: either will do.
        let both = limit(true, &["203.0.113.7"]);
        assert!(both.allows(&src("100.64.0.103", true)));
        assert!(both.allows(&src("203.0.113.7", false)));
        assert!(!both.allows(&src("203.0.113.9", false)));
    }

    #[test]
    fn saved_plainly_and_checked() {
        let l = limit(
            false,
            &[" 203.0.113.7/32 ", "198.51.100.9/24", "2001:DB8::1/128", "", "203.0.113.7"],
        );
        assert_eq!(l.addresses, ["203.0.113.7", "198.51.100.0/24", "2001:db8::1"]);
        for bad in ["nope", "203.0.113.7/33", "203.0.113.7/", "203.0.113.7/abc", "2001:db8::/129", "10.0.0.0/8/8"] {
            let e = checked(false, &[bad.to_owned()]).unwrap_err();
            assert!(e.message.contains("isn't an IP address or range"), "{bad}: {}", e.message);
        }
        let many: Vec<String> = (0..=MAX_ADDRESSES).map(|i| format!("10.0.{}.{}", i / 250, i % 250)).collect();
        assert!(checked(false, &many).is_err());
        assert!(checked(false, &many[..MAX_ADDRESSES]).is_ok());
    }

    #[test]
    fn saving_never_locks_out_the_one_saving() {
        let here = src("203.0.113.7", false);
        assert!(keeps(&limit(false, &["203.0.113.0/24"]), &here).is_ok());
        assert!(keeps(&Limit::default(), &here).is_ok());
        let e = keeps(&limit(true, &[]), &here).unwrap_err();
        assert_eq!(e.message, "This would lock you out. Add your address 203.0.113.7 first.");
        assert!(keeps(&limit(false, &["198.51.100.1"]), &here).is_err());
        assert!(keeps(&limit(true, &[]), &src("100.64.0.103", true)).is_ok());
        assert!(keeps(&limit(true, &[]), &Source::default()).is_err());
    }

    #[test]
    fn titles() {
        assert_eq!(change_title(&Limit::default()), "Let admins sign in from anywhere");
        assert_eq!(change_title(&limit(true, &[])), "Limited admin sign-in to the tailnet");
        assert_eq!(
            change_title(&limit(true, &["203.0.113.7"])),
            "Limited admin sign-in to the tailnet and 203.0.113.7"
        );
        assert_eq!(
            change_title(&limit(false, &["203.0.113.7", "2001:db8::/32"])),
            "Limited admin sign-in to 2 addresses"
        );
    }

    /// A request as Pyo gets it: its TCP peer and headers.
    fn parts(peer: &str, host: &str, headers: &[(&str, &str)]) -> Parts {
        let mut b = axum::http::Request::builder().uri("/api/auth/login");
        if !host.is_empty() {
            b = b.header("host", host);
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut r = b.body(()).unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        r.into_parts().0
    }

    fn source(peer: &str, host: &str, headers: &[(&str, &str)]) -> Source {
        Source::from_parts(&parts(peer, host, headers))
    }

    const TS: &str = "pyo.tail1234.ts.net";
    const PUBLIC: &str = "pyo.203-0-113-1.nip.io";

    #[test]
    fn the_tailnet_through_the_tailnet_gateway() {
        // The tailnet Gateway (a pod) appends the device's own address.
        assert_eq!(
            source("10.42.0.7:5000", TS, &[("x-forwarded-for", "100.64.0.103")]),
            src("100.64.0.103", true)
        );
        assert_eq!(
            source("10.42.0.7:5000", TS, &[("x-forwarded-for", "fd7a:115c:a1e0::5")]),
            src("fd7a:115c:a1e0::5", true)
        );
        // Port and a trailing dot in the host name.
        assert_eq!(
            source("10.42.0.7:5000", "pyo.tail1234.ts.net.:443", &[("x-forwarded-for", "100.64.0.103")]),
            src("100.64.0.103", true)
        );
        // HTTP/2: no Host header, the URI's authority.
        let mut r = axum::http::Request::builder()
            .uri(format!("https://{TS}/api/auth/login"))
            .header("x-forwarded-for", "100.64.0.103")
            .body(())
            .unwrap();
        r.extensions_mut()
            .insert(ConnectInfo("10.42.0.7:5000".parse::<SocketAddr>().unwrap()));
        assert_eq!(Source::from_parts(&r.into_parts().0), src("100.64.0.103", true));
    }

    #[test]
    fn spoofed_headers_from_an_untrusted_peer_never_count() {
        // Straight from the internet, whatever it claims.
        for h in [
            ("x-forwarded-for", "100.64.0.103"),
            ("cf-connecting-ip", "100.64.0.103"),
            ("x-real-ip", "100.64.0.103"),
            ("forwarded", "for=100.64.0.103"),
        ] {
            assert_eq!(source("203.0.113.9:5000", TS, &[h]), src("203.0.113.9", false), "{h:?}");
        }
        // A listed address, claimed.
        let l = limit(false, &["198.51.100.7"]);
        let s = source("203.0.113.9:5000", PUBLIC, &[("x-forwarded-for", "198.51.100.7"), ("cf-connecting-ip", "198.51.100.7")]);
        assert!(!l.allows(&s));
        // Through the public Gateway: what the client wrote sits to the
        // left of the address the Gateway appended.
        let s = source("10.42.0.7:5000", PUBLIC, &[("x-forwarded-for", "198.51.100.7, 203.0.113.9")]);
        assert_eq!(s, src("203.0.113.9", false));
        assert!(!l.allows(&s));
        // And a made-up CF-Connecting-IP from the pod network (the Gateway
        // passes on what its client sent) never counts.
        let s = source("10.42.0.7:5000", PUBLIC, &[("cf-connecting-ip", "198.51.100.7"), ("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(s, src("203.0.113.9", false));
        // A server's own end of the pod network (pyo-forward's :8000, which
        // admits local and private peers too) passes on nothing, and isn't
        // the tailnet even with a tailnet name.
        let s = source("10.42.0.1:5000", TS, &[("x-forwarded-for", "100.64.0.103")]);
        assert_eq!(s, src("10.42.0.1", false));
    }

    #[test]
    fn a_tailscale_address_off_the_tailnet_isn_t_the_tailnet() {
        // A carrier's shared NAT (100.64.0.0/10) on a public name.
        assert_eq!(
            source("10.42.0.7:5000", PUBLIC, &[("x-forwarded-for", "100.64.1.2")]),
            src("100.64.1.2", false)
        );
        assert_eq!(source("100.64.1.2:5000", PUBLIC, &[]), src("100.64.1.2", false));
        // A tailnet name with an address that isn't Tailscale's.
        assert_eq!(
            source("10.42.0.7:5000", TS, &[("x-forwarded-for", "203.0.113.9")]),
            src("203.0.113.9", false)
        );
    }

    /// Pyo Cloud: cloudflared, then its Gateway, both pods. The
    /// visitor is what Cloudflare's edge put last in X-Forwarded-For, the
    /// address it also sends as CF-Connecting-IP.
    #[test]
    fn behind_cloudflare_s_tunnel() {
        let s = source(
            "10.42.1.9:5000",
            "cloud.pyo.dev",
            &[
                ("cf-connecting-ip", "198.51.100.20"),
                ("x-forwarded-for", "6.6.6.6, 198.51.100.20, 10.42.0.33"),
            ],
        );
        assert_eq!(s, src("198.51.100.20", false));
        assert!(limit(false, &["198.51.100.0/24"]).allows(&s));
        assert!(!limit(false, &["6.6.6.6"]).allows(&s));
    }
}
