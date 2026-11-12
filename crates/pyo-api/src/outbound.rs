//! Requests to addresses users give (a registry to check a login against,
//! a backup bucket, a secret store), made from Pyo's own network. Each is
//! checked before it's used, and the client's resolver checks every address
//! it connects to again, so a name can't resolve to something else between
//! the check and the connection (DNS rebinding). Redirects aren't followed.
//!
//! Never reached: this machine (loopback), link-local addresses (cloud
//! metadata services among them), other reserved ranges, and the cluster's
//! own pod and service networks. Private networks (RFC 1918, CGNAT and
//! tailnets, IPv6 ULA) only where allowed ([`Reach`]): for a team's
//! addresses, unless the installation sets `PYO_ALLOW_PRIVATE_NETWORKS`
//! to false. A self-hosted Pyo's teams are its owner's, with a registry,
//! bucket or Vault on the LAN; a Pyo serving other people's teams (Pyo
//! Cloud) turns it off, so one team's admin can't probe the network Pyo
//! runs in. Plain http only to private addresses.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use reqwest::Url;

/// Where a request may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reach {
    /// Private networks too, not only the internet.
    pub private: bool,
}

impl Reach {
    /// For an address a team's admin gives: private networks unless the
    /// installation turns them off.
    pub fn team() -> Self {
        Self {
            private: private_allowed(),
        }
    }

    /// For an address the installation's admin gives (Pyo's own backups):
    /// private networks too.
    pub fn installation() -> Self {
        Self { private: true }
    }
}

/// Whether teams may use addresses on private networks (a registry, bucket
/// or secret store on the LAN or tailnet): on unless
/// `PYO_ALLOW_PRIVATE_NETWORKS` is false (Pyo Cloud).
pub fn private_allowed() -> bool {
    private_allowed_from(std::env::var("PYO_ALLOW_PRIVATE_NETWORKS").ok().as_deref())
}

fn private_allowed_from(v: Option<&str>) -> bool {
    !v.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// Why an address can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not an http(s) URL with a host, or with a login in it.
    Invalid,
    /// It doesn't resolve.
    Unknown,
    /// This machine, a reserved range or the cluster's own networks.
    Reserved,
    /// A private network, which this installation doesn't allow here.
    Private,
    /// Plain http to the internet.
    Plain,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refused::Invalid => "That address isn't a valid https:// URL.",
            Refused::Unknown => "Pyo can't find that address.",
            Refused::Reserved => "That address isn't allowed: it points at this server, a reserved network or the cluster's own network.",
            Refused::Private => "That address is on a private network, which this installation doesn't allow.",
            Refused::Plain => "Use https://. Plain http would send your credentials unencrypted.",
        })
    }
}

impl std::error::Error for Refused {}

/// The cluster's own pod and service networks (k3s's defaults, which Pyo's
/// installs keep): Pyo's database, stores and every service's pods.
const CLUSTER_V4: [(Ipv4Addr, u8); 2] = [
    (Ipv4Addr::new(10, 42, 0, 0), 16),
    (Ipv4Addr::new(10, 43, 0, 0), 16),
];

fn in_v4(a: Ipv4Addr, net: Ipv4Addr, bits: u8) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    u32::from(a) & mask == u32::from(net) & mask
}

/// What an address is, for these checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Public,
    Private,
    Reserved,
}

fn kind_v4(a: Ipv4Addr) -> Kind {
    let [x, y, ..] = a.octets();
    let reserved = a.is_unspecified()
        || a.is_loopback()
        || a.is_link_local() // 169.254/16: cloud metadata services
        || a.is_multicast()
        || a.is_broadcast()
        || x == 0
        || x >= 240
        || in_v4(a, Ipv4Addr::new(192, 0, 0, 0), 24) // protocol assignments (some clouds' metadata)
        || a == Ipv4Addr::new(100, 100, 100, 200) // Alibaba Cloud's metadata service
        || CLUSTER_V4.iter().any(|(n, b)| in_v4(a, *n, *b));
    if reserved {
        Kind::Reserved
    } else if a.is_private()
        || (x == 100 && (64..128).contains(&y))
        || in_v4(a, Ipv4Addr::new(198, 18, 0, 0), 15)
    {
        Kind::Private
    } else {
        Kind::Public
    }
}

fn kind_v6(a: Ipv6Addr) -> Kind {
    let s = a.segments();
    // IPv4 inside IPv6 (mapped, compatible, NAT64): as the IPv4 address.
    if let Some(v4) = a.to_ipv4_mapped() {
        return kind_v4(v4);
    }
    if s[..6] == [0; 6] && !a.is_unspecified() && !a.is_loopback() {
        return kind_v4(Ipv4Addr::from(((s[6] as u32) << 16) | s[7] as u32));
    }
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return kind_v4(Ipv4Addr::from(((s[6] as u32) << 16) | s[7] as u32));
    }
    let reserved = a.is_unspecified()
        || a.is_loopback()
        || a.is_multicast()
        || (s[0] & 0xffc0) == 0xfe80 // link-local
        || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
        || a == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254); // AWS's metadata service
    if reserved {
        Kind::Reserved
    } else if (s[0] & 0xfe00) == 0xfc00 {
        Kind::Private
    } else {
        Kind::Public
    }
}

fn kind(ip: IpAddr) -> Kind {
    match ip {
        IpAddr::V4(a) => kind_v4(a),
        IpAddr::V6(a) => kind_v6(a),
    }
}

/// Whether one address may be connected to.
fn allowed(ip: IpAddr, reach: Reach) -> Result<(), Refused> {
    match kind(ip) {
        Kind::Public => Ok(()),
        Kind::Private if reach.private => Ok(()),
        Kind::Private => Err(Refused::Private),
        Kind::Reserved => Err(Refused::Reserved),
    }
}

/// A URL's form: http(s), a host, no login.
pub fn parse(url: &str) -> Result<Url, Refused> {
    let url = Url::parse(url.trim()).map_err(|_| Refused::Invalid)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Refused::Invalid);
    }
    Ok(url)
}

/// Checks a URL against every address its host resolves to now. Call it
/// before each request to a URL (an IP address in it isn't resolved, so the
/// client's resolver never sees it); the client checks names again when it
/// connects.
pub async fn check(url: &Url, reach: Reach) -> Result<(), Refused> {
    let host = url
        .host_str()
        .ok_or(Refused::Invalid)?
        .trim_matches(['[', ']']);
    let ips: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443)))
            .await
            .map_err(|_| Refused::Unknown)?
            .map(|a| a.ip())
            .collect(),
    };
    if ips.is_empty() {
        return Err(Refused::Unknown);
    }
    for ip in &ips {
        allowed(*ip, reach)?;
    }
    if url.scheme() == "http" && !ips.iter().all(|ip| kind(*ip) == Kind::Private) {
        return Err(Refused::Plain);
    }
    Ok(())
}

/// Resolves names as usual, and refuses any that resolves to an address
/// the request mayn't reach (all of them checked, so a mix can't slip
/// one through).
struct Guard(Reach);

impl reqwest::dns::Resolve for Guard {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let (reach, host) = (self.0, name.as_str().to_owned());
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            for a in &addrs {
                allowed(a.ip(), reach)?;
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// An HTTP client for user-given addresses: names checked as it connects,
/// no redirects, no proxy from the environment (it would resolve them
/// instead).
pub fn client(reach: Reach, timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(Guard(reach)))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: Reach = Reach { private: false };
    const LAN: Reach = Reach { private: true };

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn reserved_addresses_are_never_reached() {
        for a in [
            "127.0.0.1",
            "127.8.9.1",
            "0.0.0.0",
            "0.1.2.3",
            "169.254.169.254",
            "169.254.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "192.0.0.192",
            "100.100.100.200",
            "10.42.0.15",
            "10.43.0.1",
            "10.43.255.254",
            "::1",
            "::",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "fd00:ec2::254",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "::ffff:10.43.0.1",
        ] {
            assert_eq!(allowed(ip(a), LAN), Err(Refused::Reserved), "{a}");
            assert_eq!(allowed(ip(a), TEAM), Err(Refused::Reserved), "{a}");
        }
    }

    #[test]
    fn private_networks_only_when_allowed() {
        for a in [
            "10.0.0.5",
            "10.41.255.1",
            "10.44.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.10",
            "100.64.0.1",
            "100.127.1.1",
            "198.18.0.1",
            "fd12:3456::1",
            "fc00::1",
            "::ffff:192.168.1.1",
        ] {
            assert_eq!(allowed(ip(a), TEAM), Err(Refused::Private), "{a}");
            assert_eq!(allowed(ip(a), LAN), Ok(()), "{a}");
        }
        for a in [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert_eq!(allowed(ip(a), TEAM), Ok(()), "{a}");
        }
    }

    /// Private networks are on unless turned off, like Pyo Cloud does.
    #[test]
    fn private_networks_are_on_unless_turned_off() {
        for v in [
            None,
            Some(""),
            Some("true"),
            Some("1"),
            Some("yes"),
            Some("anything"),
        ] {
            assert!(private_allowed_from(v), "{v:?}");
        }
        for v in ["false", "0", "off", "no", " FALSE ", "Off"] {
            assert!(!private_allowed_from(Some(v)), "{v}");
        }
    }

    #[test]
    fn urls_are_parsed_strictly() {
        assert!(parse("https://vault.example.com:8200/").is_ok());
        assert!(parse("http://10.0.0.5:9000").is_ok());
        for bad in [
            "ftp://x.example.com",
            "file:///etc/passwd",
            "https://user:pw@x.example.com",
            "https://user@x.example.com",
            "not a url",
            "gopher://x",
        ] {
            assert_eq!(parse(bad), Err(Refused::Invalid), "{bad}");
        }
    }

    #[tokio::test]
    async fn checks_what_the_url_points_at() {
        let url = |s: &str| parse(s).unwrap();
        assert_eq!(
            check(&url("https://169.254.169.254/latest/meta-data/"), LAN).await,
            Err(Refused::Reserved)
        );
        assert_eq!(
            check(&url("https://[fd00:ec2::254]/"), LAN).await,
            Err(Refused::Reserved)
        );
        assert_eq!(
            check(&url("http://127.0.0.1:5005/v2/"), LAN).await,
            Err(Refused::Reserved)
        );
        assert_eq!(
            check(&url("https://localhost:8443/"), LAN).await,
            Err(Refused::Reserved)
        );
        assert_eq!(
            check(&url("https://10.43.0.1/"), LAN).await,
            Err(Refused::Reserved)
        );
        assert_eq!(
            check(&url("https://192.168.1.10:5000/v2/"), TEAM).await,
            Err(Refused::Private)
        );
        assert_eq!(check(&url("http://192.168.1.10:9000/"), LAN).await, Ok(()));
        assert_eq!(check(&url("https://192.168.1.10:9000/"), LAN).await, Ok(()));
        // Plain http only on a private network.
        assert_eq!(
            check(&url("http://1.1.1.1/"), LAN).await,
            Err(Refused::Plain)
        );
        assert_eq!(check(&url("https://1.1.1.1/"), TEAM).await, Ok(()));
    }

    /// The client's resolver refuses names that resolve to reserved or
    /// (unless allowed) private addresses when it connects, whatever was
    /// checked before.
    #[tokio::test]
    async fn the_client_checks_names_as_it_connects() {
        let err = client(LAN, Duration::from_secs(5))
            .unwrap()
            .get("http://localhost:1/")
            .send()
            .await
            .unwrap_err();
        let mut source: Option<&dyn std::error::Error> = Some(&err);
        let mut refused = false;
        while let Some(e) = source {
            refused |= e.downcast_ref::<Refused>() == Some(&Refused::Reserved);
            source = e.source();
        }
        assert!(refused, "{err:?}");
    }

    /// Redirects aren't followed: a 3xx comes back as it is.
    #[tokio::test]
    async fn redirects_arent_followed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut c, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = c.read(&mut buf).await;
            let _ = c.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\n\r\n").await;
        });
        // An IP literal skips the resolver: this is the client alone, as
        // used after check().
        let res = client(LAN, Duration::from_secs(5))
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::FOUND);
    }
}
