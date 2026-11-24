//! Rate and abuse limits (signing in, sign-ups, API keys, invites,
//! webhooks, operations), shared by every replica through Postgres.
//!
//! One fixed window per (scope, key) in `pyo.rate_limits`. An attempt takes
//! a slot first, atomically (one upsert), so parallel requests can't slip
//! past the limit; credential checks give the slot back when they succeed,
//! so only failures count. Keys are client addresses (IPv6 by /64),
//! accounts, teams or keys, stored as sha256. Blocked: 429 with
//! Retry-After.
//!
//! A limit with a backoff cap locks out for longer each time its window is
//! used up again within a day (twice the last time, up to the cap). Each
//! replica also remembers, for a minute at most, what Postgres refused, so
//! a client that keeps trying once it's blocked costs no queries.

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

/// At most `max` attempts per `window_secs`. With `backoff_cap_secs`, a
/// used-up window locks out until `window_secs` × 2^n after it started (n:
/// the times it was used up within a day before), at most that long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub scope: &'static str,
    pub max: i32,
    pub window_secs: i64,
    pub backoff_cap_secs: i64,
}

const MINUTE: i64 = 60;
const MIN15: i64 = 15 * 60;
const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;

/// Failed password sign-ins per client address (password spraying).
pub const LOGIN_IP: Limit = Limit {
    scope: "login:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: DAY,
};
/// Failed password sign-ins per email, existing or not (targeted guessing).
pub const LOGIN_ACCOUNT: Limit = Limit {
    scope: "login:account",
    max: 10,
    window_secs: MIN15,
    // Short enough that someone else's wrong passwords can't keep the
    // owner out for long (a passkey or a sign-in link still works).
    backoff_cap_secs: HOUR,
};
/// Wrong two-factor codes per account (signing in, turning it off, new
/// recovery codes): a stolen password alone can't guess its way through.
/// The cap is short, like the password's, so a stranger can't keep the
/// owner out for long (a passkey still works).
pub const TWO_FACTOR_ACCOUNT: Limit = Limit {
    scope: "two-factor:account",
    max: 5,
    window_secs: MIN15,
    backoff_cap_secs: HOUR,
};
/// And per client address.
pub const TWO_FACTOR_IP: Limit = Limit {
    scope: "two-factor:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: DAY,
};
/// Passkey sign-ins started per address (each one stores a challenge; the
/// sign-in page starts one on every visit, for autofill).
pub const PASSKEY_START_IP: Limit = Limit {
    scope: "passkey-start:ip",
    max: 60,
    window_secs: MIN15,
    backoff_cap_secs: 0,
};
/// Failed passkey sign-ins per address.
pub const PASSKEY_FINISH_IP: Limit = Limit {
    scope: "passkey-finish:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: 0,
};
/// Wrong passwords before adding a passkey, per signed-in user.
pub const PASSKEY_PASSWORD_USER: Limit = Limit {
    scope: "passkey-password:user",
    max: 10,
    window_secs: MIN15,
    backoff_cap_secs: 0,
};
/// Sign-ups per address (each hashes a password, makes an account, and
/// says whether an email is taken).
pub const SIGNUP_IP: Limit = Limit {
    scope: "signup:ip",
    max: 20,
    window_secs: HOUR,
    backoff_cap_secs: 0,
};
/// "Forgot password?" emails asked for per address.
pub const FORGOT_IP: Limit = Limit {
    scope: "forgot:ip",
    max: 10,
    window_secs: HOUR,
    backoff_cap_secs: 0,
};
/// And per email, existing or not (so a mailbox can't be flooded).
pub const FORGOT_ACCOUNT: Limit = Limit {
    scope: "forgot:account",
    max: 3,
    window_secs: HOUR,
    backoff_cap_secs: 0,
};
/// Sign-in links tried per address (wrong or used ones count).
pub const SIGN_IN_LINK_IP: Limit = Limit {
    scope: "sign-in-link:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: 0,
};
/// MCP client registrations per address (unauthenticated, each one a row).
pub const OAUTH_REGISTER_IP: Limit = Limit {
    scope: "oauth-register:ip",
    max: 30,
    window_secs: HOUR,
    backoff_cap_secs: 0,
};

/// `pyo login` requests per address (unauthenticated, each one a row).
pub const CLI_LOGIN_IP: Limit = Limit {
    scope: "cli-login:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: 0,
};

/// Wrong API keys per address (each one a lookup, and a guess).
pub const API_KEY_WRONG_IP: Limit = Limit {
    scope: "api-key-wrong:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: DAY,
};
/// Requests per API key (and per assistant's MCP token).
pub const API_KEY_REQUESTS: Limit = Limit {
    scope: "api-key:key",
    max: 600,
    window_secs: MINUTE,
    backoff_cap_secs: 0,
};
/// Operations (changes) per account, from the dashboard, the API or MCP.
pub const OPS_USER: Limit = Limit {
    scope: "ops:user",
    max: 300,
    window_secs: MINUTE,
    backoff_cap_secs: 0,
};
/// Operations per team, all its members and keys together.
pub const OPS_TEAM: Limit = Limit {
    scope: "ops:team",
    max: 600,
    window_secs: MINUTE,
    backoff_cap_secs: 0,
};
/// Invites sent (and sent again) per team: each one may be an email.
pub const INVITES_TEAM: Limit = Limit {
    scope: "invite:team",
    max: 30,
    window_secs: HOUR,
    backoff_cap_secs: 0,
};
/// Deploy hook calls with a token that isn't one, per address.
pub const DEPLOY_HOOK_WRONG_IP: Limit = Limit {
    scope: "deploy-hook-wrong:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: DAY,
};
/// Deploys started through one deploy hook.
pub const DEPLOY_HOOK_CALLS: Limit = Limit {
    scope: "deploy-hook:hook",
    max: 60,
    window_secs: HOUR,
    backoff_cap_secs: 0,
};
/// Git webhook deliveries (GitHub, GitLab) whose signature or secret is
/// wrong, per address.
pub const GIT_WEBHOOK_WRONG_IP: Limit = Limit {
    scope: "git-webhook-wrong:ip",
    max: 30,
    window_secs: MIN15,
    backoff_cap_secs: DAY,
};
/// Signed Git webhook deliveries per GitHub App installation or GitLab
/// connection: a push storm can't queue builds without end.
pub const GIT_WEBHOOK_CALLS: Limit = Limit {
    scope: "git-webhook:source",
    max: 600,
    window_secs: HOUR,
    backoff_cap_secs: 0,
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
        Ok(ClientIp(ip_key(&visitor_of(&peer, &parts.headers))))
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

/// Its network address, then the prefix length unless it's one address:
/// "198.51.100.0/24", "2001:db8::1".
impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (net, max) = match self.net {
            IpAddr::V4(n) => {
                let mask = u32::MAX.checked_shl(32 - self.len).unwrap_or(0);
                (IpAddr::V4((u32::from(n) & mask).into()), 32)
            }
            IpAddr::V6(n) => {
                let mask = u128::MAX.checked_shl(128 - self.len).unwrap_or(0);
                (IpAddr::V6((u128::from(n) & mask).into()), 128)
            }
        };
        if self.len == max {
            write!(f, "{net}")
        } else {
            write!(f, "{net}/{}", self.len)
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
/// tunnel at `.0`): host processes such as `pyo-forward`'s port forward reach
/// Pyo from there, passing on whatever headers their client sent.
fn server_end(ip: &IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v) => v.octets()[3] <= 1,
        IpAddr::V6(v) => v.segments()[7] <= 1,
    }
}

/// A process on a server's host (pyo-forward's port forwards, k3s), as Pyo
/// sees it: k3s's pod network, at a server's own end.
pub fn from_server_host(ip: &IpAddr) -> bool {
    in_pod_network(ip) && server_end(ip)
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

/// k3s's pod network, where Pyo's gateway runs.
fn in_pod_network(ip: &IpAddr) -> bool {
    DEFAULT_TRUSTED_PROXIES
        .split(',')
        .filter_map(Cidr::parse)
        .any(|c| c.contains(ip))
}

/// The visitor's address behind Cloudflare: its `CF-Connecting-IP`, but
/// only when the connection comes straight from Cloudflare or cloudflared
/// (Cloudflare's ranges, or cloudflared on this machine, listed in
/// `PYO_TRUSTED_PROXIES`). Never from k3s's pod network, whatever that
/// list says: Pyo's gateway is there and passes on headers its clients
/// wrote. It tells Pyo the visitor through X-Forwarded-For instead.
/// Otherwise as [`request_ip`].
pub fn visitor_ip(peer: &str, cf_connecting_ip: &str, forwarded_for: &str, proxies: &[Cidr]) -> String {
    let parsed = |s: &str| s.trim().trim_matches(['[', ']']).parse::<IpAddr>().ok().map(|a| a.to_canonical());
    if let (Some(p), Some(cf)) = (parsed(peer), parsed(cf_connecting_ip))
        && !in_pod_network(&p)
        && (trusted(&p, proxies) || (p.is_loopback() && proxies.iter().any(|c| c.contains(&p))))
    {
        return cf.to_string();
    }
    request_ip(peer, forwarded_for, proxies)
}

/// A request's visitor, from its TCP peer and headers ([`visitor_ip`],
/// with [`trusted_proxies`]): what rate limits, sign-in protection and the
/// audit log all use.
pub fn visitor_of(peer: &str, headers: &axum::http::HeaderMap) -> String {
    let all = |k: &str| {
        headers
            .get_all(k)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",")
    };
    // Only one CF-Connecting-IP: two is no address at all.
    let cf = all("cf-connecting-ip");
    visitor_ip(peer, &cf, &all("x-forwarded-for"), trusted_proxies())
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

/// The 429 for `limit`: what there were too many of, and when to try again.
pub fn too_many_for(limit: &Limit, retry_secs: i64) -> ApiError {
    let what = match limit.scope {
        "api-key:key" | "ops:user" | "ops:team" | "deploy-hook:hook" | "git-webhook:source" => {
            "requests"
        }
        "invite:team" => "invites",
        "signup:ip" => "sign-ups",
        _ => "attempts",
    };
    too_many_of(what, retry_secs)
}

fn too_many_of(what: &str, retry_secs: i64) -> ApiError {
    let secs = retry_secs.max(1);
    let mins = (secs + 59) / 60;
    let msg = if secs < 60 {
        format!("Too many {what}. Try again in {secs} seconds.")
    } else if mins == 1 {
        format!("Too many {what}. Try again in 1 minute.")
    } else if mins < 120 {
        format!("Too many {what}. Try again in {mins} minutes.")
    } else {
        format!("Too many {what}. Try again in {} hours.", (mins + 59) / 60)
    };
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, msg).with_retry_after(secs as u64)
}

type Blocked = std::collections::HashMap<(&'static str, String), std::time::Instant>;

/// What this replica saw Postgres refuse, by (scope, key hash): blocked
/// until then. Kept a minute at most, so a key started over elsewhere
/// (its owner signed in) counts here again soon.
static BLOCKED: std::sync::Mutex<Option<Blocked>> = std::sync::Mutex::new(None);
const BLOCKED_KEEP: std::time::Duration = std::time::Duration::from_secs(60);
const BLOCKED_MAX: usize = 50_000;

fn blocked_here(scope: &'static str, hash: &str) -> Option<i64> {
    let mut g = BLOCKED.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.as_mut()?;
    let key = (scope, hash.to_owned());
    let until = *map.get(&key)?;
    let now = std::time::Instant::now();
    if until <= now {
        map.remove(&key);
        return None;
    }
    Some((until - now).as_secs() as i64 + 1)
}

fn remember_blocked(scope: &'static str, hash: &str, retry_secs: i64) {
    let mut g = BLOCKED.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(Default::default);
    let now = std::time::Instant::now();
    if map.len() >= BLOCKED_MAX {
        map.retain(|_, until| *until > now);
        if map.len() >= BLOCKED_MAX {
            return;
        }
    }
    let keep = std::time::Duration::from_secs(retry_secs.max(1) as u64).min(BLOCKED_KEEP);
    map.insert((scope, hash.to_owned()), now + keep);
}

fn forget_blocked(scope: &'static str, hash: &str) {
    let mut g = BLOCKED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = g.as_mut() {
        map.remove(&(scope, hash.to_owned()));
    }
}

/// Take one slot from each limit, in order; if any is used up, give back
/// the ones already taken and answer 429.
pub async fn take(db: &PgPool, limits: &[(Limit, &str)]) -> ApiResult<()> {
    for (i, (limit, key)) in limits.iter().enumerate() {
        if let Some(retry) = take_one(db, limit, key).await? {
            for (l, k) in &limits[..i] {
                forgive(db, l, k).await?;
            }
            return Err(too_many_for(limit, retry));
        }
    }
    Ok(())
}

/// None: taken. Some(seconds): used up until then.
async fn take_one(db: &PgPool, limit: &Limit, key: &str) -> sqlx::Result<Option<i64>> {
    let key = sha256_hex(key);
    if let Some(retry) = blocked_here(limit.scope, &key) {
        return Ok(Some(retry));
    }
    // A window that ended starts over; otherwise count up while under max,
    // and never while locked out. A window that starts a day after the
    // last one forgets the lockouts before.
    let taken: Option<i32> = sqlx::query_scalar(
        "INSERT INTO pyo.rate_limits AS r (scope, key, window_start, count) VALUES ($1, $2, now(), 1)
         ON CONFLICT (scope, key) DO UPDATE SET
           strikes = CASE WHEN r.window_start <= now() - interval '1 day' THEN 0 ELSE r.strikes END,
           count = CASE WHEN r.window_start <= now() - make_interval(secs => $3) THEN 1 ELSE r.count + 1 END,
           window_start = CASE WHEN r.window_start <= now() - make_interval(secs => $3) THEN now() ELSE r.window_start END
         WHERE coalesce(r.blocked_until <= now(), true)
           AND (r.window_start <= now() - make_interval(secs => $3) OR r.count < $4)
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
    // Used up. With a backoff, the window's first refusal locks out for
    // twice as long as the time before (the first time: to its end).
    let retry: Option<f64> = sqlx::query_scalar(
        "UPDATE pyo.rate_limits SET
           strikes = CASE WHEN $4 > 0 AND coalesce(blocked_until <= now(), true) THEN strikes + 1 ELSE strikes END,
           blocked_until = CASE WHEN $4 > 0 AND coalesce(blocked_until <= now(), true)
                                THEN window_start + make_interval(secs => least($3 * power(2, strikes), greatest($4, $3)))
                                ELSE blocked_until END
         WHERE scope = $1 AND key = $2
         RETURNING extract(epoch FROM greatest(coalesce(blocked_until, window_start), window_start + make_interval(secs => $3)) - now())::float8",
    )
    .bind(limit.scope)
    .bind(&key)
    .bind(limit.window_secs as f64)
    .bind(limit.backoff_cap_secs as f64)
    .fetch_optional(db)
    .await?;
    let retry = retry.map_or(limit.window_secs, |s| s.ceil() as i64).max(1);
    remember_blocked(limit.scope, &key, retry);
    Ok(Some(retry))
}

/// Whether `key` used up `limit`, without taking a slot: the seconds until
/// it may try again. For a check made in a transaction that shouldn't
/// write (an operation's team, which [`take_after`] counts once it's done).
pub async fn used_up(
    conn: &mut sqlx::PgConnection,
    limit: &Limit,
    key: &str,
) -> sqlx::Result<Option<i64>> {
    let key = sha256_hex(key);
    if let Some(retry) = blocked_here(limit.scope, &key) {
        return Ok(Some(retry));
    }
    let retry: Option<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM greatest(coalesce(blocked_until, window_start), window_start + make_interval(secs => $3)) - now())::float8
           FROM pyo.rate_limits
          WHERE scope = $1 AND key = $2
            AND (blocked_until > now() OR (window_start > now() - make_interval(secs => $3) AND count >= $4))",
    )
    .bind(limit.scope)
    .bind(&key)
    .bind(limit.window_secs as f64)
    .bind(limit.max)
    .fetch_optional(conn)
    .await?;
    Ok(retry.map(|s| (s.ceil() as i64).max(1)))
}

/// Counts something already done against `limit`: the next one waits when
/// this used it up.
pub async fn take_after(db: &PgPool, limit: &Limit, key: &str) {
    if let Err(e) = take_one(db, limit, key).await {
        tracing::warn!(scope = limit.scope, error = %e, "rate limit not counted");
    }
}

/// Give back one slot: the attempt succeeded, or wasn't made.
pub async fn forgive(db: &PgPool, limit: &Limit, key: &str) -> sqlx::Result<()> {
    let hash = sha256_hex(key);
    forget_blocked(limit.scope, &hash);
    sqlx::query(
        "UPDATE pyo.rate_limits SET count = greatest(count - 1, 0) WHERE scope = $1 AND key = $2",
    )
    .bind(limit.scope)
    .bind(&hash)
    .execute(db)
    .await?;
    Ok(())
}

/// Start over: e.g. the account's owner signed in.
pub async fn reset(db: &PgPool, limit: &Limit, key: &str) -> sqlx::Result<()> {
    let hash = sha256_hex(key);
    forget_blocked(limit.scope, &hash);
    sqlx::query("DELETE FROM pyo.rate_limits WHERE scope = $1 AND key = $2")
        .bind(limit.scope)
        .bind(&hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Windows that ended long ago (the longest is an hour) and lockouts over
/// for a day.
pub async fn prune(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.rate_limits WHERE window_start < now() - interval '1 day' AND coalesce(blocked_until < now() - interval '1 day', true)")
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
        let e = too_many_of("attempts", 30);
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.message, "Too many attempts. Try again in 30 seconds.");
        assert_eq!(e.retry_after, Some(30));
        assert_eq!(
            too_many_of("attempts", 61).message,
            "Too many attempts. Try again in 2 minutes."
        );
        assert_eq!(
            too_many_of("attempts", 900).message,
            "Too many attempts. Try again in 15 minutes."
        );
        assert_eq!(too_many_of("attempts", 0).retry_after, Some(1));
        let r = axum::response::IntoResponse::into_response(too_many_of("attempts", 120));
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
            TWO_FACTOR_ACCOUNT,
            TWO_FACTOR_IP,
            SIGNUP_IP,
            OAUTH_REGISTER_IP,
            CLI_LOGIN_IP,
            crate::cloud::bootstrap::WRONG_CODE_SERVER,
            crate::cloud::bootstrap::WRONG_CODE_ALL,
            API_KEY_WRONG_IP,
            API_KEY_REQUESTS,
            OPS_USER,
            OPS_TEAM,
            INVITES_TEAM,
            DEPLOY_HOOK_WRONG_IP,
            DEPLOY_HOOK_CALLS,
            GIT_WEBHOOK_WRONG_IP,
            GIT_WEBHOOK_CALLS,
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
        const { assert!(TWO_FACTOR_ACCOUNT.max < TWO_FACTOR_IP.max) };
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
            &mut req("10.42.0.7:5000", "1.2.3.4, 100.64.0.103"),
            &(),
        )
        .await
        .unwrap();
        assert_eq!(ip, "100.64.0.103");
        // Straight from the internet: the header is the client's own claim.
        let ClientIp(ip) =
            ClientIp::from_request_parts(&mut req("203.0.113.7:5000", "198.51.100.9"), &())
                .await
                .unwrap();
        assert_eq!(ip, "203.0.113.7");
    }

    /// CF-Connecting-IP counts only from Cloudflare or cloudflared on this
    /// machine, listed in PYO_TRUSTED_PROXIES. Never from the pod network,
    /// where Pyo's gateway passes on what its client sent: there the
    /// visitor is what the gateway appended to X-Forwarded-For.
    #[test]
    fn cloudflare_s_header_never_from_the_pod_network() {
        let default: Vec<Cidr> = DEFAULT_TRUSTED_PROXIES
            .split(',')
            .map(|c| Cidr::parse(c).unwrap())
            .collect();
        // A client that reaches the gateway directly makes one up: the
        // gateway (a pod) passes it on and appends the client's address.
        assert_eq!(
            visitor_ip("10.42.0.17", "1.1.1.1", "203.0.113.7", &default),
            "203.0.113.7"
        );
        assert_eq!(
            visitor_ip("[2001:cafe:42::1a]", "1.1.1.1", "2001:db8::5", &default),
            "2001:db8::5"
        );
        // Same with the pod network listed by hand (Pyo Cloud lists it).
        let listed = [Cidr::parse("10.42.0.0/16").unwrap()];
        assert_eq!(
            visitor_ip("10.42.0.17", "1.1.1.1", "203.0.113.7", &listed),
            "203.0.113.7"
        );
        // Pyo Cloud: Cloudflare's edge sets X-Forwarded-For to the visitor
        // (after anything the visitor sent), cloudflared (a pod) passes it
        // on, the gateway appends cloudflared's address. The visitor, not
        // cloudflared, and not what the visitor wrote.
        assert_eq!(
            visitor_ip("10.42.1.9", "198.51.100.20", "6.6.6.6, 198.51.100.20, 10.42.0.33", &listed),
            "198.51.100.20"
        );
        assert_eq!(
            visitor_ip("10.42.1.9", "", "198.51.100.20, 10.42.0.33", &default),
            "198.51.100.20"
        );
        // cloudflared on this machine counts only when listed
        // (cloudflare_s_header_counts_only_from_a_trusted_proxy).
        assert_eq!(visitor_ip("127.0.0.1", "203.0.113.7", "", &default), "127.0.0.1");
        // A server's own end of the pod network, even when listed.
        assert_eq!(visitor_ip("10.42.0.1", "1.1.1.1", "", &listed), "10.42.0.1");
    }

    #[tokio::test]
    async fn spoofed_cloudflare_header_through_the_gateway() {
        let mut r = axum::http::Request::builder()
            .header("cf-connecting-ip", "1.1.1.1")
            .header("x-forwarded-for", "1.1.1.1, 203.0.113.7")
            .body(())
            .unwrap();
        r.extensions_mut()
            .insert(ConnectInfo("10.42.0.7:5000".parse::<SocketAddr>().unwrap()));
        let mut parts = r.into_parts().0;
        let ClientIp(ip) = ClientIp::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(ip, "203.0.113.7");
        assert_eq!(visitor_of("10.42.0.7", &parts.headers), "203.0.113.7");
        // Two of them name no one.
        let mut h = axum::http::HeaderMap::new();
        h.append("cf-connecting-ip", "1.1.1.1".parse().unwrap());
        h.append("cf-connecting-ip", "2.2.2.2".parse().unwrap());
        assert_eq!(visitor_of("203.0.113.9", &h), "203.0.113.9");
    }

    #[test]
    fn forwarded_addresses_only_from_trusted_proxies() {
        let proxies: Vec<Cidr> = DEFAULT_TRUSTED_PROXIES
            .split(',')
            .map(|c| Cidr::parse(c).unwrap())
            .collect();
        let ip = |peer: &str, xff: &str| request_ip(peer, xff, &proxies);
        // A tailnet device, or anything on a private network, names nobody.
        assert_eq!(ip("100.64.0.103", "198.51.100.9"), "100.64.0.103");
        assert_eq!(ip("192.168.1.20", "198.51.100.9"), "192.168.1.20");
        assert_eq!(ip("127.0.0.1", "198.51.100.9"), "127.0.0.1");
        // The server's end of the pod network (pyo-forward's port forward)
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
            backoff_cap_secs: 0,
        };
        let other = Limit {
            scope: "test:b",
            max: 1,
            window_secs: 60,
            backoff_cap_secs: 0,
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
        // (This replica remembers the refusal for up to a minute.)
        forget_blocked(other.scope, &sha256_hex(&key));
        take(&db, &[(other, &key)]).await.unwrap();

        // Parallel attempts (other replicas) never exceed the limit.
        let p = Limit {
            scope: "test:c",
            max: 5,
            window_secs: 60,
            backoff_cap_secs: 0,
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

    #[test]
    fn cloudflare_s_header_counts_only_from_a_trusted_proxy() {
        let cf = [Cidr::parse("173.245.48.0/20").unwrap(), Cidr::parse("127.0.0.1/32").unwrap()];
        assert_eq!(visitor_ip("173.245.48.9", "203.0.113.7", "", &cf), "203.0.113.7");
        assert_eq!(visitor_ip("127.0.0.1", "203.0.113.7", "", &cf), "203.0.113.7");
        assert_eq!(visitor_ip("198.51.100.4", "203.0.113.7", "", &cf), "198.51.100.4");
        assert_eq!(visitor_ip("173.245.48.9", "", "", &cf), "173.245.48.9");
    }

    #[test]
    fn plain_messages_for_each_kind() {
        assert_eq!(
            too_many_for(&OPS_TEAM, 20).message,
            "Too many requests. Try again in 20 seconds."
        );
        assert_eq!(
            too_many_for(&INVITES_TEAM, 3600).message,
            "Too many invites. Try again in 60 minutes."
        );
        assert_eq!(
            too_many_for(&LOGIN_IP, 4 * 3600).message,
            "Too many attempts. Try again in 4 hours."
        );
        assert_eq!(too_many_for(&API_KEY_REQUESTS, 5).retry_after, Some(5));
    }

    async fn test_db() -> PgPool {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        db
    }

    /// Against a throwaway Postgres: a lockout doubles each time, up to its
    /// cap, a success starts over, and a quiet day forgets.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn lockouts_back_off_and_start_over() {
        let db = test_db().await;
        let l = Limit {
            scope: "test-backoff:a",
            max: 2,
            window_secs: 60,
            backoff_cap_secs: 200,
        };
        let key = format!("k{}", crate::ids::secret_token());
        let hash = sha256_hex(&key);
        // The window (and its lockout) ended: what Postgres says from now.
        let end_window = || async {
            forget_blocked(l.scope, &hash);
            sqlx::query("UPDATE pyo.rate_limits SET window_start = now() - interval '10 minutes', blocked_until = now() - interval '1 second' WHERE scope = $1 AND key = $2")
                .bind(l.scope)
                .bind(&hash)
                .execute(&db)
                .await
                .unwrap();
        };
        let retry = |e: ApiError| e.retry_after.unwrap() as i64;
        for _ in 0..2 {
            take(&db, &[(l, &key)]).await.unwrap();
        }
        let first = retry(take(&db, &[(l, &key)]).await.unwrap_err());
        assert!((55..=60).contains(&first), "{first}");
        // Refused again here without asking Postgres.
        assert!(blocked_here(l.scope, &hash).is_some());
        end_window().await;
        for _ in 0..2 {
            take(&db, &[(l, &key)]).await.unwrap();
        }
        let second = retry(take(&db, &[(l, &key)]).await.unwrap_err());
        assert!((115..=120).contains(&second), "twice as long: {second}");
        // Still locked out, though the window's minute is over.
        forget_blocked(l.scope, &hash);
        sqlx::query("UPDATE pyo.rate_limits SET window_start = window_start - interval '61 seconds', blocked_until = blocked_until - interval '61 seconds' WHERE scope = $1 AND key = $2")
            .bind(l.scope)
            .bind(&hash)
            .execute(&db)
            .await
            .unwrap();
        let left = retry(take(&db, &[(l, &key)]).await.unwrap_err());
        assert!((50..=60).contains(&left), "{left}");
        end_window().await;
        for _ in 0..2 {
            take(&db, &[(l, &key)]).await.unwrap();
        }
        let third = retry(take(&db, &[(l, &key)]).await.unwrap_err());
        assert!((195..=200).contains(&third), "up to the cap: {third}");
        // The owner signed in: back to the start.
        reset(&db, &l, &key).await.unwrap();
        take(&db, &[(l, &key)]).await.unwrap();
        // A day after the last window, earlier lockouts are forgotten.
        sqlx::query("UPDATE pyo.rate_limits SET strikes = 5, window_start = now() - interval '25 hours' WHERE scope = $1 AND key = $2")
            .bind(l.scope)
            .bind(&hash)
            .execute(&db)
            .await
            .unwrap();
        for _ in 0..2 {
            take(&db, &[(l, &key)]).await.unwrap();
        }
        let fresh = retry(take(&db, &[(l, &key)]).await.unwrap_err());
        assert!((55..=60).contains(&fresh), "{fresh}");
        // Without a backoff, a used-up window only lasts its time.
        let plain = Limit {
            scope: "test-backoff:b",
            backoff_cap_secs: 0,
            ..l
        };
        for _ in 0..2 {
            take(&db, &[(plain, &key)]).await.unwrap();
        }
        assert!(take(&db, &[(plain, &key)]).await.is_err());
        let blocked: Option<i64> = sqlx::query_scalar(
            "SELECT pyo.ms(blocked_until) FROM pyo.rate_limits WHERE scope = $1 AND key = $2",
        )
        .bind(plain.scope)
        .bind(&hash)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(blocked.is_none());
        let mut c = db.acquire().await.unwrap();
        assert!(used_up(&mut c, &plain, &key).await.unwrap().is_some());
        assert!(
            used_up(&mut c, &plain, "someone else")
                .await
                .unwrap()
                .is_none()
        );
        // (Its own scopes: shared_windows_in_postgres clears test:*.)
        sqlx::query("DELETE FROM pyo.rate_limits WHERE scope LIKE 'test-backoff:%'")
            .execute(&db)
            .await
            .unwrap();
    }

    /// Against a throwaway Postgres: wrong API keys, a key's requests, a
    /// team's operations and invites trip their limits with a plain 429,
    /// and work again once the window is over.
    #[test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    fn api_limits_trip_and_reset() {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(api_limits())
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn api_limits() {
        use serde_json::json;
        let db = test_db().await;
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::random_string(8).to_lowercase();
        let (user_id, team) = (format!("usr_rl{run}"), format!("team_rl{run}"));
        let email = format!("rl{run}@limits.test");
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, $1, '')",
        )
        .bind(&user_id)
        .bind(&email)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')")
            .bind(format!("mem_rl{run}"))
            .bind(&team)
            .bind(&user_id)
            .bind(&email)
            .execute(&db)
            .await
            .unwrap();
        let user = crate::auth::User {
            id: user_id.clone(),
            name: "rl".into(),
            email: email.clone(),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        let s = crate::rls::tests::state(db.clone());
        // Fill a window as if it had been used up already.
        let used = |limit: Limit, key: String| {
            let db = db.clone();
            async move {
                sqlx::query("INSERT INTO pyo.rate_limits (scope, key, window_start, count) VALUES ($1, $2, now(), $3)
                             ON CONFLICT (scope, key) DO UPDATE SET window_start = now(), count = $3, blocked_until = NULL")
                    .bind(limit.scope)
                    .bind(sha256_hex(&key))
                    .bind(limit.max)
                    .execute(&db)
                    .await
                    .unwrap();
            }
        };

        // Wrong API keys from one address: a 429 once used up.
        let ip = format!("198.51.100.{}", run.len());
        reset(&db, &API_KEY_WRONG_IP, &ip).await.unwrap();
        for _ in 0..API_KEY_WRONG_IP.max {
            let e = crate::auth::key_user_from(&db, "pyo_wrong", &ip)
                .await
                .unwrap_err();
            assert_eq!(e.status, StatusCode::UNAUTHORIZED);
        }
        let e = crate::auth::key_user_from(&db, "pyo_wrong", &ip)
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(e.message.starts_with("Too many attempts."), "{}", e.message);
        reset(&db, &API_KEY_WRONG_IP, &ip).await.unwrap();

        // A key's requests.
        let token = format!("pyo_{}", crate::ids::secret_token());
        sqlx::query("INSERT INTO pyo.api_tokens (id, user_id, name, prefix, token_hash, access) VALUES ($1, $2, 'k', 'pyo_', $3, 'full')")
            .bind(format!("tok_rl{run}"))
            .bind(&user_id)
            .bind(sha256_hex(&token))
            .execute(&db)
            .await
            .unwrap();
        crate::auth::key_user_from(&db, &token, &ip).await.unwrap();
        used(API_KEY_REQUESTS, sha256_hex(&token)).await;
        let e = crate::auth::key_user_from(&db, &token, &ip)
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(e.message.starts_with("Too many requests."), "{}", e.message);
        reset(&db, &API_KEY_REQUESTS, &sha256_hex(&token))
            .await
            .unwrap();
        crate::auth::key_user_from(&db, &token, &ip).await.unwrap();

        // A team's operations: counted once each is done, refused once the
        // team used its minute up, and fine again after.
        let project = |n: u32| json!({ "op": "createProject", "id": format!("prj_rl{run}{n}"), "teamId": team, "name": format!("p{n}"), "namespace": format!("p{n}-{run}") });
        crate::ops::run_json(&s, &user, project(1)).await.unwrap();
        let counted: i32 =
            sqlx::query_scalar("SELECT count FROM pyo.rate_limits WHERE scope = $1 AND key = $2")
                .bind(OPS_TEAM.scope)
                .bind(sha256_hex(&team))
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(counted >= 1);
        used(OPS_TEAM, team.clone()).await;
        let e = crate::ops::run_json(&s, &user, project(2))
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(e.message.starts_with("Too many requests."), "{}", e.message);
        reset(&db, &OPS_TEAM, &team).await.unwrap();
        crate::ops::run_json(&s, &user, project(2)).await.unwrap();

        // Invites per team.
        let invite = |n: u32| json!({ "op": "inviteMember", "id": format!("mem_rl{run}i{n}"), "teamId": team, "email": format!("i{n}{run}@limits.test"), "role": "developer" });
        crate::ops::run_json(&s, &user, invite(1)).await.unwrap();
        used(INVITES_TEAM, team.clone()).await;
        let e = crate::ops::run_json(&s, &user, invite(2))
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(e.message.starts_with("Too many invites."), "{}", e.message);
        reset(&db, &INVITES_TEAM, &team).await.unwrap();
        crate::ops::run_json(&s, &user, invite(2)).await.unwrap();

        // An account's operations, in every team.
        used(OPS_USER, user_id.clone()).await;
        let e = crate::ops::run_json(&s, &user, project(3))
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        reset(&db, &OPS_USER, &user_id).await.unwrap();

        for k in [&team, &user_id] {
            for l in [OPS_TEAM, OPS_USER, INVITES_TEAM] {
                reset(&db, &l, k).await.unwrap();
            }
        }
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pyo.users WHERE id = $1")
            .bind(&user_id)
            .execute(&db)
            .await
            .unwrap();
    }
}
