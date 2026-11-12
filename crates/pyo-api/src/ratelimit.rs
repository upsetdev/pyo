//! Rate limits for signing in, shared by every replica through Postgres.
//!
//! One fixed window per (scope, key) in `pyo.rate_limits`. An attempt takes
//! a slot first, atomically (one upsert), so parallel requests can't slip
//! past the limit; credential checks give the slot back when they succeed,
//! so only failures count. Keys are client addresses (IPv6 by /64) or
//! accounts, stored as sha256. Blocked: 429 with Retry-After.

use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{StatusCode, request::Parts},
};
use sqlx::PgPool;

use crate::{
    error::{ApiError, ApiResult},
    ids::sha256_hex,
};

/// At most `max` attempts per `window_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub scope: &'static str,
    pub max: i32,
    pub window_secs: i64,
}

const MIN15: i64 = 15 * 60;
const HOUR: i64 = 60 * 60;

/// Failed password sign-ins per client address (password spraying).
pub const LOGIN_IP: Limit = Limit {
    scope: "login:ip",
    max: 30,
    window_secs: MIN15,
};
/// Failed password sign-ins per email, existing or not (targeted guessing).
pub const LOGIN_ACCOUNT: Limit = Limit {
    scope: "login:account",
    max: 10,
    window_secs: MIN15,
};
/// Passkey sign-ins started per address (each one stores a challenge; the
/// sign-in page starts one on every visit, for autofill).
pub const PASSKEY_START_IP: Limit = Limit {
    scope: "passkey-start:ip",
    max: 60,
    window_secs: MIN15,
};
/// Failed passkey sign-ins per address.
pub const PASSKEY_FINISH_IP: Limit = Limit {
    scope: "passkey-finish:ip",
    max: 30,
    window_secs: MIN15,
};
/// Wrong passwords before adding a passkey, per signed-in user.
pub const PASSKEY_PASSWORD_USER: Limit = Limit {
    scope: "passkey-password:user",
    max: 10,
    window_secs: MIN15,
};
/// Sign-ups per address (each hashes a password, makes an account, and
/// says whether an email is taken).
pub const SIGNUP_IP: Limit = Limit {
    scope: "signup:ip",
    max: 20,
    window_secs: HOUR,
};
/// MCP client registrations per address (unauthenticated, each one a row).
pub const OAUTH_REGISTER_IP: Limit = Limit {
    scope: "oauth-register:ip",
    max: 30,
    window_secs: HOUR,
};

/// The client's address as a rate-limit key: the TCP peer, or, when the
/// peer is Pyo's gateway, the address the gateway added to X-Forwarded-For
/// ([`request_ip`]); IPv6 cut to its /64, which one client usually has whole.
pub struct ClientIp(pub String);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string())
            .unwrap_or_default();
        let xff = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",");
        Ok(ClientIp(ip_key(&request_ip(
            &peer,
            &xff,
            trusted_proxies(),
        ))))
    }
}

/// An address range, for the proxies trusted with X-Forwarded-For.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    len: u32,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Self> {
        let (ip, len) = s
            .trim()
            .split_once('/')
            .map(|(a, l)| (a, l.parse().ok()))
            .unwrap_or((s.trim(), None));
        let net: IpAddr = ip.parse().ok()?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let len = len.unwrap_or(max);
        (len <= max).then_some(Self { net, len })
    }

    pub fn contains(&self, ip: &IpAddr) -> bool {
        match (self.net, ip.to_canonical()) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = u32::MAX.checked_shl(32 - self.len).unwrap_or(0);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = u128::MAX.checked_shl(128 - self.len).unwrap_or(0);
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

/// k3s's pod network, where the gateway's proxies run.
const DEFAULT_TRUSTED_PROXIES: &str = "10.42.0.0/16,2001:cafe:42::/56";

/// The proxies whose X-Forwarded-For Pyo believes: `PYO_TRUSTED_PROXIES`
/// (comma-separated CIDRs), else k3s's pod network. Never the tailnet or
/// private ranges in general: anyone there could name any address.
pub fn trusted_proxies() -> &'static [Cidr] {
    static T: std::sync::OnceLock<Vec<Cidr>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let raw = std::env::var("PYO_TRUSTED_PROXIES")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_TRUSTED_PROXIES.to_owned());
        raw.split(',')
            .filter(|c| !c.trim().is_empty())
            .filter_map(|c| {
                Cidr::parse(c).or_else(|| {
                    tracing::warn!(cidr = c, "PYO_TRUSTED_PROXIES: not a CIDR; ignored");
                    None
                })
            })
            .collect()
    })
}

/// A server's own end of the pod network (the bridge at `.1`, flannel's
/// tunnel at `.0`): host processes such as `pyo-host`'s port forward reach
/// Pyo from there, passing on whatever headers their client sent.
fn server_end(ip: &IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v) => v.octets()[3] <= 1,
        IpAddr::V6(v) => v.segments()[7] <= 1,
    }
}

fn trusted(ip: &IpAddr, proxies: &[Cidr]) -> bool {
    proxies.iter().any(|c| c.contains(ip)) && !server_end(ip)
}

/// Who sent a request: the TCP peer, unless the peer is a trusted proxy;
/// then the address it appended to X-Forwarded-For (the rightmost entry
/// that isn't a trusted proxy itself). Nothing a client writes in the
/// header, or in X-Real-Ip, counts unless a trusted proxy vouches for it.
pub fn request_ip(peer: &str, forwarded_for: &str, proxies: &[Cidr]) -> String {
    let parse = |s: &str| {
        s.trim()
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .ok()
            .map(|a| a.to_canonical())
    };
    let Some(mut ip) = parse(peer) else {
        return peer.to_owned();
    };
    for hop in forwarded_for.rsplit(',') {
        if !trusted(&ip, proxies) {
            break;
        }
        match parse(hop) {
            Some(a) => ip = a,
            None => break,
        }
    }
    ip.to_string()
}

pub fn ip_key(ip: &str) -> String {
    match ip.parse::<IpAddr>().map(|a| a.to_canonical()) {
        Ok(IpAddr::V6(v)) => {
            let s = v.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        Ok(a) => a.to_string(),
        Err(_) => ip.to_owned(),
    }
}

/// "Too many attempts. Try again in N minutes."
pub fn too_many(retry_secs: i64) -> ApiError {
    let secs = retry_secs.max(1);
    let mins = (secs + 59) / 60;
    let msg = if mins == 1 {
        "Too many attempts. Try again in 1 minute.".to_owned()
    } else {
        format!("Too many attempts. Try again in {mins} minutes.")
    };
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, msg).with_retry_after(secs as u64)
}

/// Take one slot from each limit, in order; if any is used up, give back
/// the ones already taken and answer 429.
pub async fn take(db: &PgPool, limits: &[(Limit, &str)]) -> ApiResult<()> {
    for (i, (limit, key)) in limits.iter().enumerate() {
        if let Some(retry) = take_one(db, limit, key).await? {
            for (l, k) in &limits[..i] {
                forgive(db, l, k).await?;
            }
            return Err(too_many(retry));
        }
    }
    Ok(())
}

/// None: taken. Some(seconds): used up until then.
async fn take_one(db: &PgPool, limit: &Limit, key: &str) -> sqlx::Result<Option<i64>> {
    let key = sha256_hex(key);
    // A window that ended starts over; otherwise count up while under max.
    let taken: Option<i32> = sqlx::query_scalar(
        "INSERT INTO pyo.rate_limits AS r (scope, key, window_start, count) VALUES ($1, $2, now(), 1)
         ON CONFLICT (scope, key) DO UPDATE SET
           count = CASE WHEN r.window_start <= now() - make_interval(secs => $3) THEN 1 ELSE r.count + 1 END,
           window_start = CASE WHEN r.window_start <= now() - make_interval(secs => $3) THEN now() ELSE r.window_start END
         WHERE r.window_start <= now() - make_interval(secs => $3) OR r.count < $4
         RETURNING count",
    )
    .bind(limit.scope)
    .bind(&key)
    .bind(limit.window_secs as f64)
    .bind(limit.max)
    .fetch_optional(db)
    .await?;
    if taken.is_some() {
        return Ok(None);
    }
    let retry: Option<f64> = sqlx::query_scalar("SELECT extract(epoch FROM window_start + make_interval(secs => $3) - now())::float8 FROM pyo.rate_limits WHERE scope = $1 AND key = $2")
        .bind(limit.scope)
        .bind(&key)
        .bind(limit.window_secs as f64)
        .fetch_optional(db)
        .await?;
    Ok(Some(
        retry.map_or(limit.window_secs, |s| s.ceil() as i64).max(1),
    ))
}

/// Give back one slot: the attempt succeeded, or wasn't made.
pub async fn forgive(db: &PgPool, limit: &Limit, key: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE pyo.rate_limits SET count = greatest(count - 1, 0) WHERE scope = $1 AND key = $2",
    )
    .bind(limit.scope)
    .bind(sha256_hex(key))
    .execute(db)
    .await?;
    Ok(())
}

/// Start over: e.g. the account's owner signed in.
pub async fn reset(db: &PgPool, limit: &Limit, key: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.rate_limits WHERE scope = $1 AND key = $2")
        .bind(limit.scope)
        .bind(sha256_hex(key))
        .execute(db)
        .await?;
    Ok(())
}

/// Windows that ended long ago (the longest is an hour).
pub async fn prune(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.rate_limits WHERE window_start < now() - interval '1 day'")
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_by_64() {
        assert_eq!(ip_key("2001:db8:728:5b08::1"), "2001:db8:728:5b08::/64");
        assert_eq!(
            ip_key("2001:db8:728:5b08:aaaa:bbbb:cccc:dddd"),
            "2001:db8:728:5b08::/64"
        );
        assert_eq!(ip_key("203.0.113.7"), "203.0.113.7");
        assert_eq!(ip_key("::ffff:203.0.113.7"), "203.0.113.7");
        assert_eq!(ip_key(""), "");
    }

    #[test]
    fn message_and_retry_after() {
        let e = too_many(30);
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.message, "Too many attempts. Try again in 1 minute.");
        assert_eq!(e.retry_after, Some(30));
        assert_eq!(
            too_many(61).message,
            "Too many attempts. Try again in 2 minutes."
        );
        assert_eq!(
            too_many(900).message,
            "Too many attempts. Try again in 15 minutes."
        );
        assert_eq!(too_many(0).retry_after, Some(1));
        let r = axum::response::IntoResponse::into_response(too_many(120));
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()["retry-after"], "120");
    }

    #[test]
    fn limits_are_distinct_and_sane() {
        let all = [
            LOGIN_IP,
            LOGIN_ACCOUNT,
            PASSKEY_START_IP,
            PASSKEY_FINISH_IP,
            PASSKEY_PASSWORD_USER,
            SIGNUP_IP,
            OAUTH_REGISTER_IP,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(
                a.max > 0 && a.window_secs > 0 && a.window_secs <= 86_400,
                "{a:?}"
            );
            assert!(all[i + 1..].iter().all(|b| b.scope != a.scope), "{a:?}");
        }
        // An account is guessed at less often than a shared address may fail.
        const { assert!(LOGIN_ACCOUNT.max < LOGIN_IP.max) };
    }

    #[tokio::test]
    async fn client_ip_from_the_gateway_only() {
        let req = |peer: &str, xff: &str| {
            let mut r = axum::http::Request::builder()
                .header("x-forwarded-for", xff)
                .body(())
                .unwrap();
            r.extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            r.into_parts().0
        };
        // Through the gateway (a pod): the address it appended.
        let ClientIp(ip) = ClientIp::from_request_parts(
            &mut req("10.42.0.7:5000", "1.2.3.4, 100.101.102.103"),
            &(),
        )
        .await
        .unwrap();
        assert_eq!(ip, "100.101.102.103");
        // Straight from the internet: the header is the client's own claim.
        let ClientIp(ip) =
            ClientIp::from_request_parts(&mut req("203.0.113.7:5000", "198.51.100.9"), &())
                .await
                .unwrap();
        assert_eq!(ip, "203.0.113.7");
    }

    #[test]
    fn forwarded_addresses_only_from_trusted_proxies() {
        let proxies: Vec<Cidr> = DEFAULT_TRUSTED_PROXIES
            .split(',')
            .map(|c| Cidr::parse(c).unwrap())
            .collect();
        let ip = |peer: &str, xff: &str| request_ip(peer, xff, &proxies);
        // A tailnet device, or anything on a private network, names nobody.
        assert_eq!(ip("100.101.102.103", "198.51.100.9"), "100.101.102.103");
        assert_eq!(ip("192.168.1.20", "198.51.100.9"), "192.168.1.20");
        assert_eq!(ip("127.0.0.1", "198.51.100.9"), "127.0.0.1");
        // The server's end of the pod network (pyo-host's port forward)
        // passes a client's headers on: not trusted.
        assert_eq!(ip("10.42.0.1", "198.51.100.9"), "10.42.0.1");
        assert_eq!(ip("10.42.3.0", "198.51.100.9"), "10.42.3.0");
        // The gateway's pod: the rightmost hop, which it added; whatever the
        // client wrote to the left of it doesn't count.
        assert_eq!(ip("10.42.0.17", "6.6.6.6, 100.64.0.11"), "100.64.0.11");
        // Two of Pyo's proxies in a row: past both.
        assert_eq!(
            ip("10.42.0.17", "6.6.6.6, 100.64.0.11, 10.42.1.9"),
            "100.64.0.11"
        );
        // Nothing (or garbage) forwarded: the proxy itself.
        assert_eq!(ip("10.42.0.17", ""), "10.42.0.17");
        assert_eq!(ip("10.42.0.17", "garbage"), "10.42.0.17");
        assert_eq!(ip("[2001:cafe:42::1a]", "2001:db8::5"), "2001:db8::5");
        assert_eq!(ip("::ffff:10.42.0.17", "203.0.113.4"), "203.0.113.4");

        let custom = [Cidr::parse("203.0.113.0/24").unwrap()];
        assert_eq!(
            request_ip("203.0.113.7", "198.51.100.9", &custom),
            "198.51.100.9"
        );
        assert_eq!(
            request_ip("10.42.0.17", "198.51.100.9", &custom),
            "10.42.0.17"
        );
        assert!(Cidr::parse("10.0.0.0/33").is_none() && Cidr::parse("nope").is_none());
        assert!(
            Cidr::parse("10.1.2.3")
                .unwrap()
                .contains(&"10.1.2.3".parse().unwrap())
        );
    }

    /// Against a throwaway Postgres: `DATABASE_URL=… cargo test -p pyo-api
    /// ratelimit -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn shared_windows_in_postgres() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        let l = Limit {
            scope: "test:a",
            max: 3,
            window_secs: 60,
        };
        let other = Limit {
            scope: "test:b",
            max: 1,
            window_secs: 60,
        };
        let key = format!("k{}", crate::ids::secret_token());
        sqlx::query("DELETE FROM pyo.rate_limits WHERE scope LIKE 'test:%'")
            .execute(&db)
            .await
            .unwrap();

        for _ in 0..3 {
            take(&db, &[(l, &key)]).await.unwrap();
        }
        let e = take(&db, &[(l, &key)]).await.unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert!((1..=60).contains(&(e.retry_after.unwrap() as i64)));
        // Blocked attempts don't count up further.
        let count: i32 = sqlx::query_scalar(
            "SELECT count FROM pyo.rate_limits WHERE scope = 'test:a' AND key = $1",
        )
        .bind(sha256_hex(&key))
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(count, 3);

        // A success gives its slot back; a reset starts over.
        forgive(&db, &l, &key).await.unwrap();
        take(&db, &[(l, &key)]).await.unwrap();
        assert!(take(&db, &[(l, &key)]).await.is_err());
        reset(&db, &l, &key).await.unwrap();
        take(&db, &[(l, &key)]).await.unwrap();

        // Blocked on the second limit: the first one's slot is given back.
        take(&db, &[(other, &key)]).await.unwrap();
        let before: i32 = sqlx::query_scalar(
            "SELECT count FROM pyo.rate_limits WHERE scope = 'test:a' AND key = $1",
        )
        .bind(sha256_hex(&key))
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(take(&db, &[(l, &key), (other, &key)]).await.is_err());
        let after: i32 = sqlx::query_scalar(
            "SELECT count FROM pyo.rate_limits WHERE scope = 'test:a' AND key = $1",
        )
        .bind(sha256_hex(&key))
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(before, after);

        // An ended window starts over.
        sqlx::query("UPDATE pyo.rate_limits SET window_start = now() - interval '2 minutes' WHERE scope = 'test:b'").execute(&db).await.unwrap();
        take(&db, &[(other, &key)]).await.unwrap();

        // Parallel attempts (other replicas) never exceed the limit.
        let p = Limit {
            scope: "test:c",
            max: 5,
            window_secs: 60,
        };
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let (db, key) = (db.clone(), key.clone());
                tokio::spawn(async move { take(&db, &[(p, &key)]).await.is_ok() })
            })
            .collect();
        let mut ok = 0;
        for t in tasks {
            ok += t.await.unwrap() as i32;
        }
        assert_eq!(ok, 5);

        sqlx::query("UPDATE pyo.rate_limits SET window_start = now() - interval '2 days' WHERE scope = 'test:c'").execute(&db).await.unwrap();
        prune(&db).await.unwrap();
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pyo.rate_limits WHERE scope = 'test:c'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(left, 0);
        sqlx::query("DELETE FROM pyo.rate_limits WHERE scope LIKE 'test:%'")
            .execute(&db)
            .await
            .unwrap();
    }
}
