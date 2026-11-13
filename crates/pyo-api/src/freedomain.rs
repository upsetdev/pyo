//! Free domains: "<service>-<namespace>.203.0.113.7.nip.io" resolves to
//! 203.0.113.7, so a service gets a hostname with no DNS record to create.
//!
//! Each cluster picks the address in it ([`Mode`]):
//!
//! - Public IP: the public IPv4 address of a server that takes public
//!   traffic (a public load balancer address first, then the server's
//!   ExternalIP, then what the agent found the internet sees: locate.rs).
//!   Never a private, tailnet (CGNAT), loopback or link-local one: a cloud
//!   server behind its provider's NAT only has a private address on its
//!   network card. None while the cluster takes no public traffic.
//! - Tailscale: a server's Tailscale IPv4 address (100.x), reachable only
//!   from the tailnet. In this mode the agent publishes the websites' ports
//!   on every server's Tailscale address that runs a proxy, whatever the
//!   public traffic (`tailnetWebIps`, pyo-proto's system.rs
//!   `TAILNET_WEB_SERVICE`).
//!
//! Unset, a cluster uses its public IP when it takes public traffic and
//! the server has that address itself (on a network card, or as its
//! ExternalIP) or a load balancer gives it one. When the public IP is only
//! what the internet sees (behind NAT: a home router, some providers),
//! ports 80 and 443 may not reach the server, so Tailscale. Without an
//! address there's no free domain.
//!
//! Free domains Pyo made on another of the cluster's addresses (a private
//! one, the other mode's, one a server no longer has) or in the older
//! dashed form ("203-0-113-7.nip.io") follow it: [`rehome`] moves them in
//! place, with a routes sync (no redeploy).

use std::net::{IpAddr, Ipv4Addr};

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::error::ApiResult;

/// Routable on the internet: not private, tailnet/CGNAT, loopback,
/// link-local or unspecified (agent.rs `public_ip`).
pub fn public(ip: &IpAddr) -> bool {
    crate::agent::public_ip(ip)
}

/// A public IPv4 address, from text.
pub fn public_v4(ip: &str) -> Option<Ipv4Addr> {
    ip.trim()
        .parse::<Ipv4Addr>()
        .ok()
        .filter(|a| public(&IpAddr::V4(*a)))
}

/// A Tailscale IPv4 address (100.64.0.0/10), from text.
pub fn tailnet_v4(ip: &str) -> Option<Ipv4Addr> {
    ip.trim().parse::<Ipv4Addr>().ok().filter(|a| {
        let [a, b, ..] = a.octets();
        a == 100 && (64..=127).contains(&b)
    })
}

/// Where a cluster's free domains point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Public,
    Tailscale,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "public" => Some(Mode::Public),
            "tailscale" => Some(Mode::Tailscale),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Public => "public",
            Mode::Tailscale => "tailscale",
        }
    }
}

/// What a cluster's free domains depend on, as stored.
#[derive(Debug, Default)]
pub struct Cluster {
    pub public_traffic: bool,
    /// The mode chosen in its settings; None: by [`Cluster::mode`]'s rule.
    pub set: Option<String>,
    pub lb_ips: Vec<String>,
    /// The host's Tailscale addresses that answer websites whatever the
    /// public traffic.
    pub tailnet_web: Vec<String>,
    /// Its servers' reports, in a stable order.
    pub servers: Vec<Value>,
}

/// Servers that take public traffic, ready ones first.
fn entry(servers: &[Value]) -> impl Iterator<Item = &Value> {
    let entry = || {
        servers
            .iter()
            .filter(|r| crate::traffic::entry_server(true, r))
    };
    entry()
        .filter(|r| r["status"] == json!("ready"))
        .chain(entry().filter(|r| r["status"] != json!("ready")))
}

/// A public IPv4 address the server has itself: its ExternalIP, a node
/// address, or one on a network card (what the agent found, locate.rs).
fn direct_v4(r: &Value) -> Option<Ipv4Addr> {
    ["externalIP", "internalIP"]
        .iter()
        .filter_map(|k| r[*k].as_str())
        .chain(
            ["addresses", "nicPublicIps"]
                .iter()
                .flat_map(|k| r[*k].as_array().into_iter().flatten())
                .filter_map(Value::as_str),
        )
        .find_map(public_v4)
}

/// A server's Tailscale IPv4 address: what the agent found on it, else a
/// node address on the tailnet.
fn server_tailnet(r: &Value) -> Option<Ipv4Addr> {
    r["tailnetIp"]
        .as_str()
        .and_then(tailnet_v4)
        .or_else(|| addresses(r).find_map(|a| tailnet_v4(&a.to_string())))
}

impl Cluster {
    /// Its public IPv4 address, for free domains and the DNS records Pyo
    /// suggests: a public load balancer address, else a server's that takes
    /// public traffic. None while it takes none.
    pub fn public_ip(&self) -> Option<Ipv4Addr> {
        if !self.public_traffic {
            return None;
        }
        self.lb_ips.iter().find_map(|i| public_v4(i)).or_else(|| {
            entry(&self.servers)
                .find_map(|r| crate::traffic::public_families(r).0)
                .and_then(|ip| ip.parse().ok())
        })
    }

    /// Its public IPv4 address when the internet reaches it directly: a
    /// public load balancer address, or one a server that takes public
    /// traffic has itself (on a network card, or as its ExternalIP). Not
    /// one only the outbound lookup found (behind NAT).
    pub fn direct_public_ip(&self) -> Option<Ipv4Addr> {
        if !self.public_traffic {
            return None;
        }
        self.lb_ips
            .iter()
            .find_map(|i| public_v4(i))
            .or_else(|| entry(&self.servers).find_map(direct_v4))
    }

    /// Its public IP is known only from what the internet sees: the server
    /// is behind NAT, so ports 80 and 443 reach it only if forwarded.
    pub fn behind_nat(&self) -> bool {
        self.public_ip().is_some() && self.direct_public_ip().is_none()
    }

    /// Its Tailscale IPv4 address that answers websites: where the agent
    /// published them, else (not yet) a server's that runs a proxy.
    pub fn tailnet_ip(&self) -> Option<Ipv4Addr> {
        self.tailnet_web
            .iter()
            .find_map(|i| tailnet_v4(i))
            .or_else(|| entry(&self.servers).find_map(server_tailnet))
    }

    /// The mode in use: the one chosen, else public when it takes public
    /// traffic and the internet reaches its public IP directly, else
    /// Tailscale.
    pub fn mode(&self) -> Mode {
        self.set
            .as_deref()
            .and_then(Mode::parse)
            .unwrap_or(if self.direct_public_ip().is_some() {
                Mode::Public
            } else {
                Mode::Tailscale
            })
    }

    /// The address free domains point at now, if any.
    pub fn address(&self) -> Option<Ipv4Addr> {
        match self.mode() {
            Mode::Public => self.public_ip(),
            Mode::Tailscale => self.tailnet_ip(),
        }
    }

    /// Every address of its own: where a free domain Pyo made could point.
    fn known(&self) -> Vec<IpAddr> {
        let mut known: Vec<IpAddr> = self.servers.iter().flat_map(addresses).collect();
        known.extend(
            self.lb_ips
                .iter()
                .chain(&self.tailnet_web)
                .filter_map(|ip| ip.parse::<IpAddr>().ok()),
        );
        known
    }

    /// For the dashboard: the addresses and the mode, as the cluster's
    /// `publicIp`, `tailnetIp`, `freeDomainAddress`, `freeDomainIp` and
    /// `behindNat`.
    pub fn shown(&self) -> Value {
        let s = |ip: Option<Ipv4Addr>| json!(ip.map(|i| i.to_string()));
        json!({
            "publicIp": s(self.public_ip()),
            "tailnetIp": s(self.tailnet_ip()),
            "freeDomainAddress": self.mode().as_str(),
            "freeDomainIp": s(self.address()),
            "behindNat": self.behind_nat(),
        })
    }
}

/// "nginx-web.203.0.113.7.nip.io": the label at most 63 characters, so the
/// whole name stays well within 253.
pub fn hostname(service: &str, namespace: &str, ip: Ipv4Addr) -> String {
    let label: String = format!("{service}-{namespace}").chars().take(63).collect();
    format!("{}.{ip}.nip.io", label.trim_end_matches('-'))
}

/// A free domain's parts: its label(s), address, and whether it's in the
/// older dashed form. "app.203.0.113.7.nip.io" and "app.203-0-113-7.nip.io"
/// → ("app", 203.0.113.7).
fn split(host: &str) -> Option<(&str, Ipv4Addr, bool)> {
    parts(host.strip_suffix(".nip.io")?)
}

/// The address in a nip.io or sslip.io name, in either form.
pub fn ip_in(host: &str) -> Option<Ipv4Addr> {
    let rest = host
        .strip_suffix(".nip.io")
        .or_else(|| host.strip_suffix(".sslip.io"))?;
    parts(rest).map(|(_, ip, _)| ip)
}

/// "app.203.0.113.7" or "app.203-0-113-7" → ("app", 203.0.113.7, dashed).
fn parts(rest: &str) -> Option<(&str, Ipv4Addr, bool)> {
    let parts: Vec<&str> = rest.rsplitn(5, '.').collect();
    if parts.len() == 5
        && let Ok(ip) = format!("{}.{}.{}.{}", parts[3], parts[2], parts[1], parts[0]).parse()
        && !parts[4].is_empty()
    {
        return Some((parts[4], ip, false));
    }
    let (label, ip) = rest.rsplit_once('.')?;
    Some((label, ip.replace('-', ".").parse().ok()?, true))
}

/// The hostname a free domain moves to: one on another of the cluster's
/// own addresses (`known`), or in the dashed form, goes to `to`. Any other
/// hostname stays.
pub fn moved(host: &str, known: &[IpAddr], to: Ipv4Addr) -> Option<String> {
    let (label, ip, dashed) = split(host)?;
    let ours = ip == to || known.contains(&IpAddr::V4(ip));
    (ours && (ip != to || dashed)).then(|| format!("{label}.{to}.nip.io"))
}

/// Every address a server's report lists.
fn addresses(r: &Value) -> impl Iterator<Item = IpAddr> + '_ {
    [
        "internalIP",
        "externalIP",
        "publicIp",
        "publicIpv4",
        "publicIpv6",
        "tailnetIp",
    ]
    .iter()
    .filter_map(|k| r[*k].as_str())
    .chain(
        ["addresses", "nicPublicIps"]
            .iter()
            .flat_map(|k| r[*k].as_array().into_iter().flatten())
            .filter_map(Value::as_str),
    )
    .filter_map(|ip| ip.parse().ok())
}

/// Addresses from stored capabilities: `key` "loadBalancerIps" or
/// "tailnetWebIps".
pub fn ips(caps: &Value, key: &str) -> Vec<String> {
    caps[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// A cluster as stored, and its team.
pub async fn load(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
) -> ApiResult<Option<(String, Cluster)>> {
    let row: Option<(String, bool, Option<String>, Value)> = sqlx::query_as(
        "SELECT team_id, public_traffic, free_domain_address, capabilities FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((team_id, public_traffic, set, caps)) = row else {
        return Ok(None);
    };
    let servers: Vec<Value> =
        sqlx::query_scalar("SELECT report FROM pyo.servers WHERE cluster_id = $1 ORDER BY name")
            .bind(cluster_id)
            .fetch_all(&mut **tx)
            .await?;
    Ok(Some((
        team_id,
        Cluster {
            public_traffic,
            set,
            lb_ips: ips(&caps, "loadBalancerIps"),
            tailnet_web: ips(&caps, "tailnetWebIps"),
            servers,
        },
    )))
}

/// A cluster's free-domain address now.
pub async fn for_cluster(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
) -> ApiResult<Option<Ipv4Addr>> {
    Ok(load(tx, cluster_id).await?.and_then(|(_, c)| c.address()))
}

/// Moves the cluster's free domains made on another of its addresses, or
/// in the dashed form, to its free-domain address, live: a routes sync per
/// service that runs. Whether a sync was queued (wake the agent).
pub async fn rehome(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<bool> {
    let Some((team_id, c)) = load(tx, cluster_id).await? else {
        return Ok(false);
    };
    let Some(to) = c.address() else {
        return Ok(false);
    };
    let known = c.known();
    let services: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT id, name, spec->'domains' FROM pyo.services
          WHERE cluster_id = $1 AND NOT deleting AND (spec->'domains')::text LIKE '%.nip.io%'",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut synced = false;
    for (id, name, mut domains) in services {
        let mut changes = vec![];
        for d in domains.as_array_mut().into_iter().flatten() {
            let host = d["hostname"].as_str().unwrap_or("").to_owned();
            let Some(new) = moved(&host, &known, to) else {
                continue;
            };
            if taken(tx, &new).await? {
                continue;
            }
            move_domain(d, &host, &new);
            changes.push((host, new));
        }
        if changes.is_empty() {
            continue;
        }
        sqlx::query(
            "UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1",
        )
        .bind(&id)
        .bind(&domains)
        .execute(&mut **tx)
        .await?;
        synced |= crate::routes::changed(tx, &id, true).await?.is_some();
        for (from, new) in changes {
            crate::ops::log_activity(
                tx,
                &team_id,
                "Pyo",
                "domain.moved",
                "info",
                &format!("{from} is now {new}"),
                json!({ "kind": "service", "id": id, "label": name }),
                json!({ "serviceId": id }),
            )
            .await?;
        }
    }
    Ok(synced)
}

/// Whether a service already has the hostname.
async fn taken(tx: &mut Transaction<'_, Postgres>, host: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
                         WHERE NOT a.deleting AND d->>'hostname' = $1)",
    )
    .bind(host)
    .fetch_one(&mut **tx)
    .await?)
}

/// A domain on its new hostname: its DNS and certificate start over, and
/// HTTP is redirected to HTTPS only where the internet reaches it (as for
/// a new domain), when that changed.
fn move_domain(d: &mut Value, from: &str, to: &str) {
    let Some(o) = d.as_object_mut() else {
        return;
    };
    o.insert("hostname".into(), json!(to));
    o.insert("status".into(), json!("pending_dns"));
    o.insert("tls".into(), json!("pending"));
    for k in ["error", "tlsError", "dnsCheckedAt"] {
        o.remove(k);
    }
    let private = crate::deploy::private_host(to);
    if crate::deploy::private_host(from) != private {
        o.insert("redirectHttps".into(), json!(!private));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn classifies_addresses() {
        let p = |s: &str| public(&s.parse().unwrap());
        // Private, tailnet/CGNAT, loopback, link-local, unspecified: not public.
        for ip in [
            "10.3.8.13",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.48",
            "100.64.0.1",
            "100.101.67.2",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "::",
            "fd7a:115c:a1e0::6930:4303",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!p(ip), "{ip} isn't public");
        }
        for ip in [
            "129.226.158.31",
            "172.15.0.1",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "1.1.1.1",
            "2606:4700::1111",
        ] {
            assert!(p(ip), "{ip} is public");
        }
        assert_eq!(public_v4(" 129.226.158.31 "), Some(v4("129.226.158.31")));
        assert_eq!(public_v4("10.3.8.13"), None);
        // nip.io names need IPv4.
        assert_eq!(public_v4("2606:4700::1111"), None);
        assert_eq!(public_v4("lb.example.com"), None);
        assert_eq!(tailnet_v4("100.101.67.2"), Some(v4("100.101.67.2")));
        for ip in [
            "100.128.0.1",
            "100.63.0.1",
            "10.3.8.13",
            "129.226.158.31",
            "fd7a:115c:a1e0::1",
        ] {
            assert_eq!(tailnet_v4(ip), None, "{ip}");
        }
    }

    /// A cloud server behind its provider's NAT, on a tailnet: the
    /// gateway's load balancer has the private address, the agent found
    /// the public and the Tailscale ones.
    fn nat_server() -> Value {
        json!({ "name": "dev-ten-sin0", "status": "ready", "internalIP": "10.3.8.13", "addresses": ["10.3.8.13"],
                "publicIp": "129.226.158.31", "publicIpv4": "129.226.158.31", "tailnetIp": "100.101.67.2" })
    }

    fn cluster(public_traffic: bool, set: Option<&str>, servers: Vec<Value>) -> Cluster {
        Cluster {
            public_traffic,
            set: set.map(str::to_owned),
            lb_ips: if public_traffic {
                vec!["10.3.8.13".into()]
            } else {
                vec![]
            },
            tailnet_web: vec![],
            servers,
        }
    }

    /// A server with its public address on its network card (a VPS).
    fn direct_server() -> Value {
        json!({ "name": "vps", "status": "ready", "internalIP": "10.0.0.5", "addresses": ["10.0.0.5"],
                "publicIpv4": "203.0.113.7", "nicPublicIps": ["203.0.113.7"], "tailnetIp": "100.64.0.9" })
    }

    #[test]
    fn public_mode_uses_the_public_address_never_a_private_one() {
        let c = cluster(true, Some("public"), vec![nat_server()]);
        assert_eq!(c.mode(), Mode::Public);
        assert_eq!(c.address(), Some(v4("129.226.158.31")));
        // No public address known: none, never the private one.
        let private = json!({ "name": "a", "status": "ready", "internalIP": "10.3.8.13" });
        let c = cluster(true, Some("public"), vec![private]);
        assert_eq!(c.address(), None);
        assert_eq!(c.public_ip(), None);
        // ExternalIP set on the server.
        let ext = json!({ "name": "a", "status": "ready", "internalIP": "10.0.0.5", "externalIP": "203.0.113.9" });
        assert_eq!(
            cluster(true, None, vec![ext]).address(),
            Some(v4("203.0.113.9"))
        );
        // MetalLB with a public pool comes first.
        let mut c = cluster(true, None, vec![nat_server()]);
        c.lb_ips = vec!["fd00::1".into(), "203.0.113.50".into()];
        assert_eq!(c.address(), Some(v4("203.0.113.50")));
        // Public traffic off: no public address, even when chosen.
        let c = cluster(false, Some("public"), vec![nat_server()]);
        assert_eq!((c.mode(), c.address()), (Mode::Public, None));
    }

    #[test]
    fn tailscale_mode_uses_the_tailnet_address() {
        let c = cluster(true, Some("tailscale"), vec![nat_server()]);
        assert_eq!(c.mode(), Mode::Tailscale);
        assert_eq!(c.address(), Some(v4("100.101.67.2")));
        // A server joined over Tailscale: its node address.
        let joined = json!({ "name": "b", "status": "ready", "internalIP": "100.64.0.7" });
        assert_eq!(
            cluster(true, Some("tailscale"), vec![joined.clone()]).address(),
            Some(v4("100.64.0.7"))
        );
        // Public traffic off: the agent publishes the websites there all the same.
        assert_eq!(
            cluster(false, Some("tailscale"), vec![nat_server()]).address(),
            Some(v4("100.101.67.2"))
        );
        // Where the agent published them comes first.
        let mut c = cluster(false, None, vec![joined, nat_server()]);
        c.tailnet_web = vec!["100.101.67.2".into()];
        assert_eq!(
            (c.mode(), c.address()),
            (Mode::Tailscale, Some(v4("100.101.67.2")))
        );
        // Not on a server that runs a proxy (build-only).
        let builder =
            json!({ "name": "a", "status": "ready", "builder": true, "tailnetIp": "100.64.0.8" });
        assert_eq!(
            cluster(true, Some("tailscale"), vec![builder]).address(),
            None
        );
        // Not on a tailnet at all.
        let lan = json!({ "name": "a", "status": "ready", "internalIP": "192.168.1.48" });
        assert_eq!(cluster(true, Some("tailscale"), vec![lan]).address(), None);
    }

    /// Public by default only where the internet reaches the public IP
    /// directly: on the server's network card, its ExternalIP, or a load
    /// balancer's. Behind NAT (the address only the lookup found), Tailscale.
    #[test]
    fn default_is_public_only_when_reached_directly() {
        let c = cluster(true, None, vec![direct_server()]);
        assert_eq!(
            (c.mode(), c.address(), c.behind_nat()),
            (Mode::Public, Some(v4("203.0.113.7")), false)
        );
        // A public node address (k3s on a server with its public IP).
        let node = json!({ "name": "a", "status": "ready", "internalIP": "203.0.113.8", "publicIpv4": "203.0.113.8" });
        assert_eq!(cluster(true, None, vec![node]).mode(), Mode::Public);
        // ExternalIP.
        let ext = json!({ "name": "a", "status": "ready", "internalIP": "10.0.0.5", "externalIP": "203.0.113.9", "publicIpv4": "198.51.100.1" });
        assert_eq!(cluster(true, None, vec![ext]).mode(), Mode::Public);
        // A public load balancer address.
        let mut c = cluster(true, None, vec![nat_server()]);
        c.lb_ips = vec!["203.0.113.50".into()];
        assert_eq!((c.mode(), c.behind_nat()), (Mode::Public, false));
        // Behind NAT (this VPS, a home router): Tailscale, and the hint.
        let c = cluster(true, None, vec![nat_server()]);
        assert_eq!(
            (c.mode(), c.address(), c.behind_nat()),
            (Mode::Tailscale, Some(v4("100.101.67.2")), true)
        );
        assert_eq!(
            c.shown(),
            json!({ "publicIp": "129.226.158.31", "tailnetIp": "100.101.67.2", "freeDomainAddress": "tailscale",
                    "freeDomainIp": "100.101.67.2", "behindNat": true })
        );
        // Public can still be chosen there.
        let c = cluster(true, Some("public"), vec![nat_server()]);
        assert_eq!(
            (c.mode(), c.address()),
            (Mode::Public, Some(v4("129.226.158.31")))
        );
        // Public traffic off.
        let c = cluster(false, None, vec![direct_server()]);
        assert_eq!((c.mode(), c.behind_nat()), (Mode::Tailscale, false));
        // No public address known (yet).
        let tailnet_only = json!({ "name": "a", "status": "ready", "internalIP": "10.3.8.13", "tailnetIp": "100.101.67.2" });
        let c = cluster(true, None, vec![tailnet_only]);
        assert_eq!(
            (c.mode(), c.address(), c.behind_nat()),
            (Mode::Tailscale, Some(v4("100.101.67.2")), false)
        );
        // A choice stays, whatever is known.
        assert_eq!(
            cluster(true, Some("tailscale"), vec![direct_server()]).mode(),
            Mode::Tailscale
        );
    }

    #[test]
    fn only_servers_taking_public_traffic_count() {
        let builder = json!({ "name": "a-builder", "status": "ready", "builder": true, "publicIpv4": "198.51.100.1" });
        let down = json!({ "name": "b", "status": "not_ready", "publicIpv4": "198.51.100.2" });
        let up = json!({ "name": "c", "status": "ready", "publicIpv4": "198.51.100.3" });
        let c = |s: Vec<Value>| cluster(true, Some("public"), s).address();
        assert_eq!(
            c(vec![builder.clone(), down.clone(), up]),
            Some(v4("198.51.100.3"))
        );
        // Only a server that isn't ready: better than nothing.
        assert_eq!(c(vec![builder.clone(), down]), Some(v4("198.51.100.2")));
        assert_eq!(c(vec![builder]), None);
    }

    #[test]
    fn hostnames() {
        assert_eq!(
            hostname("whoami", "web", v4("129.226.158.31")),
            "whoami-web.129.226.158.31.nip.io"
        );
        assert_eq!(
            hostname("app-", "", v4("100.101.67.2")),
            "app.100.101.67.2.nip.io"
        );
        let h = hostname(&"a".repeat(300), "ns", v4("255.255.255.255"));
        assert_eq!(h.split('.').next().unwrap().len(), 63);
        assert!(h.len() <= 253 && h.split('.').all(|l| l.len() <= 63));
        assert!(crate::util::hostname(&h));
        assert!(!crate::deploy::private_host(&hostname(
            "a",
            "b",
            v4("129.226.158.31")
        )));
        assert!(crate::deploy::private_host(&hostname(
            "a",
            "b",
            v4("100.101.67.2")
        )));
    }

    #[test]
    fn free_domains_follow_the_clusters_address() {
        let known: Vec<IpAddr> = ["10.3.8.13", "100.101.67.2", "129.226.158.31"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let public = v4("129.226.158.31");
        let m = |h: &str, to| moved(h, &known, to);
        // The private address, in either form, and the older dashed form.
        assert_eq!(
            m("whoami-web.10-3-8-13.nip.io", public).as_deref(),
            Some("whoami-web.129.226.158.31.nip.io")
        );
        assert_eq!(
            m("whoami-web.10.3.8.13.nip.io", public).as_deref(),
            Some("whoami-web.129.226.158.31.nip.io")
        );
        assert_eq!(
            m("whoami-web.129-226-158-31.nip.io", public).as_deref(),
            Some("whoami-web.129.226.158.31.nip.io")
        );
        // Switched to Tailscale, and back.
        let tailnet = v4("100.101.67.2");
        assert_eq!(
            m("app.129.226.158.31.nip.io", tailnet).as_deref(),
            Some("app.100.101.67.2.nip.io")
        );
        assert_eq!(
            m("app.100.101.67.2.nip.io", public).as_deref(),
            Some("app.129.226.158.31.nip.io")
        );
        // Labels with dots stay whole.
        assert_eq!(
            m("a.b.10.3.8.13.nip.io", public).as_deref(),
            Some("a.b.129.226.158.31.nip.io")
        );
        // Already there, someone else's address, or not a free domain: stays.
        assert_eq!(m("whoami-web.129.226.158.31.nip.io", public), None);
        assert_eq!(m("app.192.168.1.2.nip.io", public), None);
        assert_eq!(m("app.192-168-1-2.nip.io", public), None);
        assert_eq!(m("shop.example.com", public), None);
        assert_eq!(m("10-3-8-13.nip.io", public), None);
        assert_eq!(m("10.3.8.13.nip.io", public), None);
    }

    #[test]
    fn a_moved_domain_starts_over_on_https() {
        let mut d = json!({ "id": "dom_1", "hostname": "a.10-3-8-13.nip.io", "status": "active", "tls": "issued", "tlsMode": "auto",
                            "redirectHttps": false, "dnsCheckedAt": 1, "error": "x", "createdAt": 5 });
        move_domain(&mut d, "a.10-3-8-13.nip.io", "a.129.226.158.31.nip.io");
        assert_eq!(
            d,
            json!({ "id": "dom_1", "hostname": "a.129.226.158.31.nip.io", "status": "pending_dns", "tls": "pending", "tlsMode": "auto",
                    "redirectHttps": true, "createdAt": 5 })
        );
        // To the tailnet: no redirect to a self-signed certificate.
        move_domain(&mut d, "a.129.226.158.31.nip.io", "a.100.101.67.2.nip.io");
        assert_eq!(d["redirectHttps"], false);
        // Public to public: the choice stays.
        d["redirectHttps"] = json!(false);
        move_domain(&mut d, "a.129-226-158-31.nip.io", "a.129.226.158.31.nip.io");
        assert_eq!(d["redirectHttps"], false);
    }
}
