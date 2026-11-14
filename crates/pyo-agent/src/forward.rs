//! `pyo-agent forward`: pyo-forward's port forwards on the machine Pyo runs
//! on (host network). Each port listens on every address and admits a
//! connection by its real peer address, never a header:
//!
//! - `local`: loopback (127.0.0.0/8, ::1);
//! - `private`: 10/8, 172.16/12, 192.168/16, fc00::/7;
//! - `tailnet`: Tailscale's 100.64.0.0/10 and fd7a:115c:a1e0::/48.
//!
//! A peer no rule admits (a public address, NAT or not) is closed at once.
//! Rules are `PORT/CLASS[,CLASS…]=HOST:PORT`, first match wins, e.g.
//! `8000/local,private,tailnet=pyo.pyo-system.svc.cluster.local:8000`.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Local,
    Private,
    Tailnet,
}

impl Class {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Class::Local),
            "private" => Some(Class::Private),
            "tailnet" => Some(Class::Tailnet),
            _ => None,
        }
    }
}

/// The class of a peer address (IPv4-mapped IPv6 as IPv4), if any.
pub fn class_of(ip: IpAddr) -> Option<Class> {
    match ip.to_canonical() {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            if v.is_loopback() {
                Some(Class::Local)
            } else if a == 100 && (64..=127).contains(&b) {
                Some(Class::Tailnet)
            } else if v.is_private() {
                Some(Class::Private)
            } else {
                None
            }
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            if v.is_loopback() {
                Some(Class::Local)
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                Some(Class::Tailnet)
            } else if s[0] & 0xfe00 == 0xfc00 {
                Some(Class::Private)
            } else {
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub port: u16,
    pub classes: Vec<Class>,
    pub target: String,
}

pub fn parse_rule(s: &str) -> anyhow::Result<Rule> {
    let bad = || anyhow::anyhow!("a rule is PORT/CLASS[,CLASS]=HOST:PORT, not {s:?}");
    let (head, target) = s.split_once('=').ok_or_else(bad)?;
    let (port, classes) = head.split_once('/').ok_or_else(bad)?;
    let classes = classes
        .split(',')
        .map(Class::parse)
        .collect::<Option<Vec<_>>>()
        .filter(|c| !c.is_empty())
        .ok_or_else(bad)?;
    if !target
        .rsplit_once(':')
        .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok())
    {
        return Err(bad());
    }
    Ok(Rule {
        port: port.parse().map_err(|_| bad())?,
        classes,
        target: target.to_owned(),
    })
}

/// Where a peer on `port` goes, if anywhere.
pub fn route<'a>(rules: &'a [Rule], port: u16, peer: IpAddr) -> Option<&'a str> {
    let class = class_of(peer)?;
    rules
        .iter()
        .find(|r| r.port == port && r.classes.contains(&class))
        .map(|r| r.target.as_str())
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let rules: Vec<Rule> = args
        .iter()
        .map(|a| parse_rule(a))
        .collect::<anyhow::Result<_>>()?;
    if rules.is_empty() {
        anyhow::bail!("usage: pyo-agent forward PORT/CLASS[,CLASS]=HOST:PORT ...");
    }
    let rules: &'static [Rule] = Box::leak(rules.into_boxed_slice());
    let mut ports: Vec<u16> = rules.iter().map(|r| r.port).collect();
    ports.sort_unstable();
    ports.dedup();
    let mut tasks = tokio::task::JoinSet::new();
    for port in ports {
        // Every address, IPv6 and IPv4 at once; IPv4 alone without IPv6.
        let listener =
            match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))).await {
                Ok(l) => l,
                Err(_) => TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?,
            };
        tracing::info!(port, "forwarding");
        tasks.spawn(async move {
            loop {
                let (conn, peer) = match listener.accept().await {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(port, error = %e, "accept");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let Some(target) = route(rules, port, peer.ip()) else {
                    drop(conn);
                    continue;
                };
                tokio::spawn(pipe(conn, target));
            }
        });
    }
    while let Some(r) = tasks.join_next().await {
        r?;
    }
    Ok(())
}

async fn pipe(mut conn: TcpStream, target: &'static str) {
    let _ = conn.set_nodelay(true);
    let up = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(target)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(target, error = %e, "couldn't connect");
            return;
        }
        Err(_) => {
            tracing::warn!(target, "connecting timed out");
            return;
        }
    };
    let _ = up.set_nodelay(true);
    let mut up = up;
    let _ = tokio::io::copy_bidirectional(&mut conn, &mut up).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classes_by_peer_address() {
        for (a, c) in [
            ("127.0.0.1", Some(Class::Local)),
            ("::1", Some(Class::Local)),
            ("::ffff:127.0.0.1", Some(Class::Local)),
            ("10.1.2.3", Some(Class::Private)),
            ("172.20.0.1", Some(Class::Private)),
            ("192.168.1.5", Some(Class::Private)),
            ("::ffff:192.168.1.5", Some(Class::Private)),
            ("fd00::1", Some(Class::Private)),
            ("100.64.0.1", Some(Class::Tailnet)),
            ("100.127.255.254", Some(Class::Tailnet)),
            ("fd7a:115c:a1e0::1", Some(Class::Tailnet)),
            ("100.128.0.1", None),
            ("172.32.0.1", None),
            ("203.0.113.7", None),
            ("::ffff:203.0.113.7", None),
            ("2001:db8::1", None),
            ("fe80::1", None),
        ] {
            assert_eq!(class_of(ip(a)), c, "{a}");
        }
    }

    #[test]
    fn rules_route_by_port_and_class() {
        let rules: Vec<Rule> = [
            "8000/local,private,tailnet=pyo.pyo-system.svc.cluster.local:8000",
            "5006/tailnet=pyo-registry-door.pyo-system.svc.cluster.local:5006",
        ]
        .iter()
        .map(|r| parse_rule(r).unwrap())
        .collect();
        assert_eq!(
            route(&rules, 8000, ip("192.168.1.5")),
            Some("pyo.pyo-system.svc.cluster.local:8000")
        );
        assert_eq!(route(&rules, 8000, ip("203.0.113.7")), None);
        assert_eq!(
            route(&rules, 5006, ip("100.70.0.2")),
            Some("pyo-registry-door.pyo-system.svc.cluster.local:5006")
        );
        // The door is never on the LAN, this machine or the public address.
        assert_eq!(route(&rules, 5006, ip("10.0.0.2")), None);
        assert_eq!(route(&rules, 5006, ip("127.0.0.1")), None);
        assert_eq!(route(&rules, 5006, ip("203.0.113.7")), None);
        for bad in [
            "8000=x:1",
            "x/local=a:1",
            "8000/public=a:1",
            "8000/local=a",
            "8000/local=:1",
        ] {
            assert!(parse_rule(bad).is_err(), "{bad}");
        }
    }
}
