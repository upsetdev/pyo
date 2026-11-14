//! Which of a service's variables point at a database of its import, so
//! Pyo can connect the new database to it before its first deploy (as the
//! database's Connect action does), and which point at a database Pyo
//! can't match, so the service isn't deployed against the old one.
//!
//! Each platform says what it can (a Railway `${{Postgres.DATABASE_URL}}`
//! reference, a Vercel variable's `contentHint`), then every value is
//! read as a database would be reached: a URL whose host, port and
//! database name are one database's (inside the platform or from outside),
//! a host (with its port) that is one database's, and the port, user,
//! password and database name next to such a host (`PGHOST`, `PGPORT`…).
//! Coolify's `{{environment.X}}` and Dokploy's `${{environment.X}}` come
//! here resolved, so they're read by their value.
//!
//! Conservative: a URL to a public host that isn't the platform's (a
//! database outside it) is left alone. One to a name only the platform
//! knows (no dot, `.internal`, a private address, the platform's own host,
//! one of its databases' hosts) that matches none of the import's
//! databases holds the service.

use std::collections::BTreeMap;
use std::net::IpAddr;

use reqwest::Url;

use super::{ImportConnect, ImportDatabase};

/// Where one of their databases is reached and with which login: what its
/// variables would hold. Never sent back.
#[derive(Debug, Clone, Default)]
pub struct DbAddress {
    /// (host, port), lowercase: inside the platform and from outside.
    pub endpoints: Vec<(String, u16)>,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl DbAddress {
    /// Adds what a URL of it says: its host and port, and the login and
    /// database name when they aren't known yet.
    pub fn add_url(&mut self, url: &str) {
        let Some(u) = parse(url) else { return };
        self.add_host(&u.host, u.port);
        if self.user.is_empty() {
            self.user = u.user;
        }
        if self.password.is_empty() {
            self.password = u.password;
        }
        if self.database.is_empty() {
            self.database = u.database;
        }
    }

    pub fn add_host(&mut self, host: &str, port: u16) {
        let host = host.trim().trim_end_matches('.').to_lowercase();
        if host.is_empty() || port == 0 {
            return;
        }
        if !self.endpoints.iter().any(|(h, p)| *h == host && *p == port) {
            self.endpoints.push((host, port));
        }
    }
}

/// What a variable holds of a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Its URL.
    Url,
    /// Its URL past any pooler.
    Direct,
    Host,
    /// `host:port`.
    HostPort,
    Port,
    User,
    Password,
    Database,
    /// A KV database's REST API (Vercel's KV variables).
    RestUrl,
    RestToken,
    RestReadOnly,
}

/// The variable of Pyo's database a role becomes, as `connect` maps it
/// (`databases::mapped`): None when Pyo's has none (left as it is).
pub fn pyo_var(target: &str, role: Role) -> Option<&'static str> {
    use Role::*;
    Some(match (target, role) {
        ("postgres", Url) => "DATABASE_URL",
        ("postgres", Direct) => "DATABASE_URL_DIRECT",
        ("postgres", Host) => "POSTGRES_HOST",
        ("postgres", HostPort) => "POSTGRES_HOST:POSTGRES_PORT",
        ("postgres", Port) => "POSTGRES_PORT",
        ("postgres", User) => "POSTGRES_USER",
        ("postgres", Password) => "POSTGRES_PASSWORD",
        ("postgres", Database) => "POSTGRES_DATABASE",
        ("mysql", Url | Direct) => "DATABASE_URL",
        ("mysql", Host) => "MYSQL_HOST",
        ("mysql", HostPort) => "MYSQL_HOST:MYSQL_PORT",
        ("mysql", Port) => "MYSQL_PORT",
        ("mysql", User) => "MYSQL_USER",
        ("mysql", Password) => "MYSQL_PASSWORD",
        ("mysql", Database) => "MYSQL_DATABASE",
        ("valkey", Url | Direct) => "REDIS_URL",
        ("valkey", Host) => "REDIS_HOST",
        ("valkey", HostPort) => "REDIS_HOST:REDIS_PORT",
        ("valkey", Port) => "REDIS_PORT",
        ("valkey", Password) => "REDIS_PASSWORD",
        ("valkey", RestUrl) => "KV_REST_API_URL",
        ("valkey", RestToken) => "KV_REST_API_TOKEN",
        ("valkey", RestReadOnly) => "KV_REST_API_READ_ONLY_TOKEN",
        _ => return None,
    })
}

/// A database URL, read.
struct DbUrl {
    /// "postgres", "mysql", "redis" or "mongodb".
    family: &'static str,
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

/// The engine family of a URL scheme Pyo reads as a database's.
fn family(scheme: &str) -> Option<&'static str> {
    Some(match scheme {
        "postgres" | "postgresql" => "postgres",
        "mysql" => "mysql",
        "redis" | "rediss" => "redis",
        "mongodb" | "mongodb+srv" => "mongodb",
        _ => return None,
    })
}

fn default_port(family: &str) -> u16 {
    match family {
        "postgres" => 5432,
        "mysql" => 3306,
        "redis" => 6379,
        _ => 27017,
    }
}

/// The engine family of one of their databases' engine.
fn engine_family(engine: &str) -> &str {
    match engine {
        "mariadb" => "mysql",
        "keydb" | "dragonfly" | "valkey" => "redis",
        e => e,
    }
}

/// Undoes a URL part's percent-encoding.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(x) = u8::from_str_radix(hex, 16)
        {
            out.push(x);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse(value: &str) -> Option<DbUrl> {
    let v = value.trim();
    let scheme = v.split_once("://")?.0.to_ascii_lowercase();
    let family = family(&scheme)?;
    let u = Url::parse(v).ok()?;
    let host = u.host_str()?.trim_matches(['[', ']']).to_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(DbUrl {
        family,
        port: u.port().unwrap_or(default_port(family)),
        host,
        user: decode(u.username()),
        password: decode(u.password().unwrap_or("")),
        database: decode(u.path().trim_start_matches('/')),
    })
}

/// A name only the platform's network knows, or an address of it: a
/// variable that points there and isn't one of the import's databases
/// can't be left as it is.
fn platform_host(host: &str, platform_hosts: &[String], dbs: &[ImportDatabase]) -> bool {
    let host = host.to_lowercase();
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        let private = match ip {
            IpAddr::V4(a) => {
                a.is_private()
                    || a.is_loopback()
                    || a.is_link_local()
                    // 100.64.0.0/10: carrier-grade NAT, tailnets.
                    || (a.octets()[0] == 100 && (a.octets()[1] & 0xc0) == 64)
            }
            IpAddr::V6(a) => a.is_loopback() || (a.segments()[0] & 0xfe00) == 0xfc00,
        };
        if private {
            return true;
        }
    } else if !host.contains('.')
        || [".internal", ".local", ".localhost", ".lan"]
            .iter()
            .any(|s| host.ends_with(s))
    {
        return true;
    }
    platform_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host))
        || dbs
            .iter()
            .any(|d| d.address.endpoints.iter().any(|(h, _)| *h == host))
}

/// A key that names a host: `PGHOST`, `DB_HOST`, `REDIS_HOSTNAME`.
fn host_key(key: &str) -> Option<&str> {
    let k = key.to_ascii_uppercase();
    ["HOSTNAME", "HOST"]
        .iter()
        .find(|s| k.ends_with(*s))
        .map(|s| &key[..key.len() - s.len()])
}

/// The URL one isn't through a pooler: `DATABASE_URL_DIRECT`,
/// `POSTGRES_URL_NON_POOLING`, `DATABASE_URL_UNPOOLED`.
fn direct_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.contains("DIRECT") || k.contains("NON_POOLING") || k.contains("UNPOOLED")
}

/// What a service's variables say about the import's databases: the ones
/// to connect, and why it can't be deployed as it is.
#[derive(Debug, Default, PartialEq)]
pub struct Links {
    pub connects: Vec<ImportConnect>,
    pub holds: Vec<String>,
}

fn they(keys: &[String]) -> (String, &'static str) {
    (
        keys.join(", "),
        if keys.len() == 1 { "points" } else { "point" },
    )
}

/// Links a service's variables (key and the value to read, references
/// resolved) to `dbs`, the databases of its import. `explicit`: what the
/// platform itself says a variable is (its database's key and role).
/// `platform_hosts`: the platform's own addresses.
pub fn link(
    env: &[(String, String)],
    explicit: &BTreeMap<String, (String, Role)>,
    dbs: &[ImportDatabase],
    platform_hosts: &[String],
) -> Links {
    // (variable, database, role)
    let mut found: Vec<(String, usize, Role)> = vec![];
    let mut unmatched: Vec<String> = vec![];
    let by_key = |k: &str| dbs.iter().position(|d| d.key == k);
    for (key, value) in env {
        if let Some((db, role)) = explicit.get(key)
            && let Some(i) = by_key(db)
        {
            found.push((key.clone(), i, *role));
            continue;
        }
        let v = value.trim();
        if let Some(u) = parse(v) {
            let same = |d: &ImportDatabase| {
                engine_family(&d.engine) == u.family
                    && d.address.endpoints.contains(&(u.host.clone(), u.port))
                    && (u.family == "redis"
                        || u.database.is_empty()
                        || d.address.database.is_empty()
                        || u.database == d.address.database)
            };
            let hits: Vec<usize> = (0..dbs.len()).filter(|&i| same(&dbs[i])).collect();
            match hits.as_slice() {
                [i] => {
                    let role = if direct_key(key) {
                        Role::Direct
                    } else {
                        Role::Url
                    };
                    found.push((key.clone(), *i, role));
                }
                _ if hits.len() > 1 || platform_host(&u.host, platform_hosts, dbs) => {
                    unmatched.push(key.clone())
                }
                // A database outside the platform: reached from Pyo the same.
                _ => {}
            }
            continue;
        }
        // A host, or host:port, of one of them.
        let lower = v.to_lowercase();
        let (host, port) = match lower.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() && !h.contains(':') => {
                (h.to_owned(), p.parse::<u16>().ok())
            }
            _ => (lower.clone(), None),
        };
        if host.is_empty() || host.contains('/') || host.contains(char::is_whitespace) {
            continue;
        }
        let hits: Vec<usize> = (0..dbs.len())
            .filter(|&i| {
                dbs[i].address.endpoints.iter().any(|(h, p)| {
                    *h == host
                        && port.is_none_or(|x| x == *p)
                        // A bare address could be anything's: only names.
                        && (port.is_some() || h.parse::<IpAddr>().is_err())
                })
            })
            .collect();
        match (hits.as_slice(), port) {
            ([i], Some(_)) => found.push((key.clone(), *i, Role::HostPort)),
            ([i], None) if host_key(key).is_some() => found.push((key.clone(), *i, Role::Host)),
            ([], _) => {}
            _ => unmatched.push(key.clone()),
        }
    }
    // The port, login and database name next to a host: PGHOST's PGPORT,
    // PGUSER, PGPASSWORD, PGDATABASE; DB_HOST's DB_PORT, DB_USER...
    let hosts: Vec<(String, usize, String)> = found
        .iter()
        .filter(|(_, _, r)| *r == Role::Host)
        .filter_map(|(k, i, _)| host_key(k).map(|p| (p.to_owned(), *i, k.clone())))
        .collect();
    for (prefix, i, host_var) in hosts {
        let d = &dbs[i].address;
        let host = env
            .iter()
            .find(|(k, _)| *k == host_var)
            .map(|(_, v)| v.trim().to_lowercase())
            .unwrap_or_default();
        let ports: Vec<String> = d
            .endpoints
            .iter()
            .filter(|(h, _)| *h == host)
            .map(|(_, p)| p.to_string())
            .collect();
        for (key, value) in env {
            if found.iter().any(|(k, _, _)| k == key) || !key.starts_with(&prefix) {
                continue;
            }
            let rest = key[prefix.len()..].to_ascii_uppercase();
            let rest = rest.trim_start_matches('_');
            let v = value.trim();
            let role = match rest {
                "PORT" if ports.iter().any(|p| p == v) => Role::Port,
                "USER" | "USERNAME" if !d.user.is_empty() && v == d.user => Role::User,
                "PASSWORD" | "PASS" if !d.password.is_empty() && v == d.password => Role::Password,
                "DATABASE" | "DB" | "NAME" | "DBNAME"
                    if !d.database.is_empty() && v == d.database =>
                {
                    Role::Database
                }
                _ => continue,
            };
            found.push((key.clone(), i, role));
        }
    }

    let mut out = Links::default();
    let mut not_imported: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (key, i, role) in found {
        let d = &dbs[i];
        let Some(target) = d.target.as_deref().filter(|_| d.action != "manual") else {
            not_imported.entry(i).or_default().push(key);
            continue;
        };
        // One Pyo's database has no value for (a KV database's user):
        // left as it is.
        let Some(to) = pyo_var(target, role) else {
            continue;
        };
        match out.connects.iter_mut().find(|c| c.database == d.key) {
            Some(c) => {
                c.env.insert(key, to.to_owned());
            }
            None => out.connects.push(ImportConnect {
                database: d.key.clone(),
                env: BTreeMap::from([(key, to.to_owned())]),
            }),
        }
    }
    if !unmatched.is_empty() {
        let (keys, verb) = they(&unmatched);
        out.holds.push(format!(
            "{keys} {verb} at a database Pyo couldn't match. Connect one, then deploy."
        ));
    }
    for (i, keys) in not_imported {
        let (keys, verb) = they(&keys);
        out.holds.push(format!(
            "{keys} {verb} at {}, which Pyo doesn't import. Connect a database, then deploy.",
            dbs[i].source_name
        ));
    }
    out
}

/// Why a service whose variables point at `db` of its import can't be
/// deployed with this import: the database isn't picked.
pub fn not_brought(keys: &[String], db: &ImportDatabase) -> String {
    let (keys, verb) = they(keys);
    format!(
        "{keys} {verb} at {}, which this import doesn't bring. Import it, or connect a database, then deploy.",
        db.source_name
    )
}
