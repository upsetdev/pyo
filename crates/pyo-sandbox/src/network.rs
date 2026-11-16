//! Sandbox networking: a tap device per VM on Firecracker, or a network
//! namespace with a veth pair per sandbox on gVisor. An address per
//! sandbox from a private pool, NAT out of the host's default route, and
//! one nftables table the daemon owns.
//!
//! ## Addresses
//!
//! Each sandbox with network gets lease `n`: the tap `pyosbx<n>` and the
//! `n`th /30 of the pool (`198.18.0.0/16` by default, part of the
//! benchmarking range that nothing routes). The host end of the /30 is the
//! guest's gateway. The daemon refuses to turn networking on when the pool
//! overlaps a route or address on the host, the tailnet, k3s, Docker or
//! other special ranges.
//!
//! The guest's address isn't baked into the VM. envd sets it, the default
//! route and `/etc/resolv.conf` when the daemon sends `setNetwork` over
//! vsock after every start and restore. So one template snapshot taken
//! with a network interface restores into any lease: Firecracker's
//! `network_overrides` points the snapshot's interface at the new tap.
//!
//! "No network" means no tap and no interface in the VM at all. Template
//! snapshots come with and without an interface (see
//! `pyo_proto::sandbox::SNAPSHOT_NET_TAG`).
//!
//! ## Rules
//!
//! One table, `inet pyo_sandbox`, replaced as a whole and atomically on
//! every daemon start. Sandboxes change only set elements after that.
//!
//! - Traffic from a tap to the host itself is dropped (the input hook).
//!   That covers every host address, the public one included.
//! - Forwarded traffic from a tap must come from the tap's own guest
//!   address (the `guests` set, so a guest can't spoof another), must be
//!   IPv4, and may not go to the tailnet (`tailscale0` and
//!   100.64.0.0/10), another sandbox, 169.254.169.254 and the rest of
//!   link-local, RFC1918 or other special ranges (the `blocked` set).
//!   DNS to the configured resolvers is let through first.
//! - `egress` is a verdict map from tap to a chain, empty for now. Per
//!   sandbox allow and deny lists slot in there later: a chain per
//!   sandbox that drops or returns.
//! - Nothing new comes in to a tap. Replies do.
//! - Sandbox traffic leaving through any other interface is masqueraded.
//!
//! The forward chain runs at `filter - 10`, before iptables' `FORWARD`
//! (`filter`), so its drops always come first.
//!
//! ## Other firewalls
//!
//! A packet passes only when every forward chain at the hook lets it
//! through, so an `accept` here doesn't get past another table that drops.
//! Docker and ufw set iptables' `FORWARD` policy to DROP. When it is, the
//! daemon adds two rules to `DOCKER-USER` (when Docker made it) or else to
//! `FORWARD`: accept what comes from a tap, and accept established or
//! related traffic back to one. What reaches them already passed this
//! table. They carry the comment `pyo-sandbox`, so every daemon start
//! replaces them and uninstall removes them. The policy itself is never
//! changed.
//!
//! The daemon also turns on IPv4 forwarding, and turns off IPv6 on taps.
//!
//! ## gVisor
//!
//! On gVisor the lease's link is a veth pair instead of a tap. Its host end
//! has the tap's name (`pyosbx<n>`) and the host address, so every rule
//! above holds as it is. The other end is `eth0` in the network namespace
//! `pyosbx<n>`, with the guest address and the default route already set.
//! runsc's netstack takes them from there (`--network=sandbox`), so nothing
//! is set inside the sandbox. runsc moves them out of the namespace as it
//! takes them, so a restore puts them back first (`readdress`). The
//! namespace goes with the lease.

use std::collections::BTreeSet;
use std::fmt;
use std::net::Ipv4Addr;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Mutex;

use anyhow::{Context, anyhow, bail};
use pyo_proto::sandbox::wire::GuestNetwork;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const TABLE: &str = "pyo_sandbox";
pub const TAP_PREFIX: &str = "pyosbx";
/// The interface name inside the guest, and Firecracker's `iface_id`.
pub const GUEST_IFACE: &str = "eth0";
/// The MAC every guest gets. Each tap is its own link, so they can share.
pub const GUEST_MAC: &str = "06:00:c6:12:00:02";

/// What a lease's link is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// A tap Firecracker opens.
    Tap,
    /// A veth pair whose other end is in the lease's network namespace
    /// (gVisor).
    Veth,
}

/// Ranges a sandbox never reaches, besides the pool itself.
pub const ALWAYS_BLOCKED: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// Ranges the pool may not overlap: the tailnet, k3s, Docker and the
/// ranges that never route.
const POOL_CLASHES: &[(&str, &str)] = &[
    ("0.0.0.0/8", "this network"),
    ("10.42.0.0/16", "k3s pods"),
    ("10.43.0.0/16", "k3s services"),
    ("100.64.0.0/10", "the tailnet"),
    ("127.0.0.0/8", "loopback"),
    ("169.254.0.0/16", "link-local"),
    ("172.16.0.0/12", "Docker"),
    ("224.0.0.0/3", "multicast and reserved"),
];

/// The `network` part of the daemon's config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfig {
    /// Turns sandbox networking off. Sandboxes then need `network: none`.
    #[serde(default)]
    pub disabled: bool,
    /// Where sandbox addresses come from, a /30 each.
    #[serde(default = "default_pool")]
    pub pool: Cidr,
    /// The resolvers written to each guest's `/etc/resolv.conf`. Sandboxes
    /// may reach them on port 53 even when they're in a blocked range.
    #[serde(default = "default_dns")]
    pub dns: Vec<Ipv4Addr>,
    /// More ranges sandboxes never reach, like Pyo's API or a private
    /// network the server is on.
    #[serde(default)]
    pub blocked: Vec<Cidr>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            disabled: false,
            pool: default_pool(),
            dns: default_dns(),
            blocked: Vec::new(),
        }
    }
}

fn default_pool() -> Cidr {
    Cidr::new(Ipv4Addr::new(198, 18, 0, 0), 16)
}

fn default_dns() -> Vec<Ipv4Addr> {
    vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
}

/// An IPv4 range like `198.18.0.0/16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Cidr {
    /// The range holding `addr`, with host bits cleared.
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Cidr {
        let prefix = prefix.min(32);
        Cidr {
            addr: Ipv4Addr::from(u32::from(addr) & mask(prefix)),
            prefix,
        }
    }

    pub fn first(&self) -> u32 {
        u32::from(self.addr)
    }

    pub fn last(&self) -> u32 {
        self.first() | !mask(self.prefix)
    }

    pub fn contains(&self, a: Ipv4Addr) -> bool {
        (self.first()..=self.last()).contains(&u32::from(a))
    }

    pub fn overlaps(&self, o: &Cidr) -> bool {
        self.first() <= o.last() && o.first() <= self.last()
    }
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for Cidr {
    type Err = String;
    fn from_str(s: &str) -> Result<Cidr, String> {
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (
                a,
                p.parse::<u8>()
                    .map_err(|_| format!("bad prefix in {s:?}"))?,
            ),
            None => (s, 32),
        };
        if p > 32 {
            return Err(format!("bad prefix in {s:?}"));
        }
        let addr: Ipv4Addr = a
            .parse()
            .map_err(|_| format!("{s:?} isn't an IPv4 range"))?;
        Ok(Cidr::new(addr, p))
    }
}

impl Serialize for Cidr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Cidr, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// One sandbox's slot in the pool. Saved in `sandbox.json`, so a restarted
/// daemon keeps the same tap and addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub index: u32,
    pub tap: String,
    /// The host end of the /30, the guest's gateway.
    pub host: Ipv4Addr,
    pub guest: Ipv4Addr,
}

impl Lease {
    pub fn guest_network(&self, dns: &[Ipv4Addr]) -> GuestNetwork {
        GuestNetwork {
            iface: GUEST_IFACE.into(),
            address: self.guest,
            prefix: 30,
            gateway: self.host,
            dns: dns.to_vec(),
        }
    }
}

pub struct Network {
    pub cfg: NetworkConfig,
    pub link: LinkKind,
    /// Who owns the taps: the jail user, so Firecracker can open them after
    /// it dropped root.
    tap_uid: u32,
    tap_gid: u32,
    used: Mutex<BTreeSet<u32>>,
}

impl Network {
    /// Checks the host can do sandbox networking and the pool is free.
    /// Changes nothing on the host. The table and IPv4 forwarding come with
    /// [`Network::sync`].
    pub async fn open(
        cfg: NetworkConfig,
        link: LinkKind,
        tap_uid: u32,
        tap_gid: u32,
    ) -> anyhow::Result<Network> {
        if cfg.disabled {
            bail!("Sandbox networking is turned off in the config.");
        }
        if !crate::config::is_root() {
            bail!("Sandbox networking needs root.");
        }
        check_pool(&cfg.pool)?;
        for (tool, arg) in [("ip", "-V"), ("nft", "--version")] {
            let ok = Command::new(tool)
                .arg(arg)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                bail!("Sandbox networking needs `{tool}`, and it isn't installed.");
            }
        }
        if link == LinkKind::Tap && !std::path::Path::new("/dev/net/tun").exists() {
            bail!("Sandbox networking needs /dev/net/tun.");
        }
        let routes = run_out("ip", &["-j", "-4", "route", "show", "table", "all"]).await?;
        if let Some(clash) = route_clash(&cfg.pool, &routes)? {
            bail!(
                "The sandbox address pool {} overlaps {clash} on this server. Pick another pool in the config.",
                cfg.pool
            );
        }
        Ok(Network {
            cfg,
            link,
            tap_uid,
            tap_gid,
            used: Mutex::new(BTreeSet::new()),
        })
    }

    /// How many sandboxes the pool holds.
    pub fn size(&self) -> u32 {
        let n = 1u64 << (32 - self.cfg.pool.prefix as u64);
        (n / 4).min(u32::MAX as u64) as u32
    }

    pub fn lease_at(&self, index: u32) -> Lease {
        let base = self.cfg.pool.first() + index * 4;
        Lease {
            index,
            tap: format!("{TAP_PREFIX}{index}"),
            host: Ipv4Addr::from(base + 1),
            guest: Ipv4Addr::from(base + 2),
        }
    }

    pub fn guest_network(&self, lease: &Lease) -> GuestNetwork {
        lease.guest_network(&self.cfg.dns)
    }

    /// The lease's network namespace on gVisor, as runsc takes it.
    pub fn netns_path(&self, lease: &Lease) -> Option<String> {
        (self.link == LinkKind::Veth).then(|| format!("{NETNS_DIR}/{}", lease.tap))
    }

    fn used(&self) -> std::sync::MutexGuard<'_, BTreeSet<u32>> {
        self.used.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A new lease with its tap up and its guest address let through.
    pub async fn acquire(&self) -> anyhow::Result<Lease> {
        let index = {
            let mut used = self.used();
            let i = (0..self.size())
                .find(|i| !used.contains(i))
                .ok_or_else(|| anyhow!("the sandbox address pool {} is full", self.cfg.pool))?;
            used.insert(i);
            i
        };
        let lease = self.lease_at(index);
        let made = async {
            self.make_tap(&lease).await?;
            nft(&format!(
                "add element inet {TABLE} guests {{ {} }}\n",
                guest_element(&lease)
            ))
            .await
        }
        .await;
        if let Err(e) = made {
            self.release(&lease).await;
            return Err(e);
        }
        Ok(lease)
    }

    /// Removes the lease's tap and rules and frees it. Never fails: what's
    /// already gone stays gone.
    pub async fn release(&self, lease: &Lease) {
        let _ = nft(&format!(
            "delete element inet {TABLE} guests {{ {} }}\n",
            guest_element(lease)
        ))
        .await;
        if let Err(e) = run("ip", &["link", "del", &lease.tap]).await
            && tap_exists(&lease.tap)
        {
            tracing::warn!(tap = %lease.tap, error = %format!("{e:#}"), "can't remove the tap");
        }
        if netns_exists(&lease.tap)
            && let Err(e) = run("ip", &["netns", "del", &lease.tap]).await
        {
            tracing::warn!(netns = %lease.tap, error = %format!("{e:#}"), "can't remove the network namespace");
        }
        self.used().remove(&lease.index);
    }

    async fn make_tap(&self, lease: &Lease) -> anyhow::Result<()> {
        if tap_exists(&lease.tap) {
            run("ip", &["link", "del", &lease.tap]).await?;
        }
        if self.link == LinkKind::Veth {
            return self.make_veth(lease).await;
        }
        let (uid, gid) = (self.tap_uid.to_string(), self.tap_gid.to_string());
        run(
            "ip",
            &[
                "tuntap", "add", "dev", &lease.tap, "mode", "tap", "user", &uid, "group", &gid,
            ],
        )
        .await?;
        let _ = std::fs::write(
            format!("/proc/sys/net/ipv6/conf/{}/disable_ipv6", lease.tap),
            "1",
        );
        let host = format!("{}/30", lease.host);
        run("ip", &["addr", "add", &host, "dev", &lease.tap]).await?;
        run("ip", &["link", "set", &lease.tap, "up"]).await
    }

    /// Gives the namespace's `eth0` its address and default route back.
    /// runsc takes them out of the namespace when it starts a sandbox, and
    /// `runsc restore` skips an `eth0` without an address, so a resumed
    /// sandbox would have no network. Run before each restore.
    pub async fn readdress(&self, lease: &Lease) -> anyhow::Result<()> {
        if self.link != LinkKind::Veth {
            return Ok(());
        }
        let ns = lease.tap.as_str();
        let guest = format!("{}/30", lease.guest);
        let gw = lease.host.to_string();
        let steps: [&[&str]; 2] = [
            &["addr", "replace", &guest, "dev", GUEST_IFACE],
            &["route", "replace", "default", "via", &gw],
        ];
        for step in steps {
            let mut args = vec!["-n", ns];
            args.extend_from_slice(step);
            run("ip", &args)
                .await
                .context("giving the sandbox its address back")?;
        }
        Ok(())
    }

    /// The lease's namespace with `eth0` in it, and the host end of the
    /// pair named like a tap. See "gVisor" above.
    async fn make_veth(&self, lease: &Lease) -> anyhow::Result<()> {
        let ns = lease.tap.as_str();
        if netns_exists(ns) {
            run("ip", &["netns", "del", ns]).await?;
        }
        run("ip", &["netns", "add", ns]).await?;
        let made = async {
            run(
                "ip",
                &[
                    "link",
                    "add",
                    &lease.tap,
                    "type",
                    "veth",
                    "peer",
                    "name",
                    GUEST_IFACE,
                    "netns",
                    ns,
                ],
            )
            .await?;
            let _ = std::fs::write(
                format!("/proc/sys/net/ipv6/conf/{}/disable_ipv6", lease.tap),
                "1",
            );
            let host = format!("{}/30", lease.host);
            let guest = format!("{}/30", lease.guest);
            let gw = lease.host.to_string();
            run("ip", &["addr", "add", &host, "dev", &lease.tap]).await?;
            run("ip", &["link", "set", &lease.tap, "up"]).await?;
            let steps: [&[&str]; 5] = [
                &["sysctl", "-qw", "net.ipv6.conf.all.disable_ipv6=1"],
                &["ip", "addr", "add", &guest, "dev", GUEST_IFACE],
                &["ip", "link", "set", GUEST_IFACE, "address", GUEST_MAC, "up"],
                &["ip", "link", "set", "lo", "up"],
                &["ip", "route", "add", "default", "via", &gw],
            ];
            for step in steps {
                let mut args = vec!["netns", "exec", ns];
                args.extend_from_slice(step);
                // sysctl may be missing. The rest may not fail.
                if let Err(e) = run("ip", &args).await
                    && step[0] != "sysctl"
                {
                    return Err(e);
                }
            }
            anyhow::Ok(())
        }
        .await;
        if made.is_err() {
            let _ = run("ip", &["link", "del", &lease.tap]).await;
            let _ = run("ip", &["netns", "del", ns]).await;
        }
        made
    }

    /// Puts the table in place for the sandboxes a restarted daemon kept,
    /// in one atomic step. Each lease comes with whether its VM runs. Makes
    /// taps that went missing for paused sandboxes, and removes taps no
    /// sandbox owns.
    pub async fn sync(&self, leases: &[(Lease, bool)]) -> anyhow::Result<()> {
        let ls: Vec<Lease> = leases.iter().map(|(l, _)| l.clone()).collect();
        nft(&self.ruleset(&ls))
            .await
            .context("installing the sandbox nftables table")?;
        std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
            .context("turning on IPv4 forwarding")?;
        if let Some(chain) = sync_forward_rules().await? {
            tracing::info!(
                %chain,
                "iptables' FORWARD policy is DROP, so sandbox taps are let through in {chain}"
            );
        }
        for (lease, running) in leases {
            self.used().insert(lease.index);
            if !running && !tap_exists(&lease.tap) {
                self.make_tap(lease).await?;
            }
        }
        let links = run_out("ip", &["-j", "link", "show"]).await?;
        for tap in stray_taps(&links, &ls)? {
            tracing::info!(%tap, "removing a tap no sandbox owns");
            let _ = run("ip", &["link", "del", &tap]).await;
        }
        for ns in stray_netns(&dir_entries(NETNS_DIR), &ls) {
            tracing::info!(%ns, "removing a network namespace no sandbox owns");
            let _ = run("ip", &["netns", "del", &ns]).await;
        }
        Ok(())
    }

    /// The whole table, as an `nft -f` script that replaces the old one in
    /// one transaction.
    pub fn ruleset(&self, leases: &[Lease]) -> String {
        let c = &self.cfg;
        let mut blocked: Vec<Cidr> = ALWAYS_BLOCKED
            .iter()
            .map(|s| s.parse().expect("valid range"))
            .collect();
        blocked.push(c.pool);
        blocked.extend(c.blocked.iter().copied());
        let blocked = merge(blocked);
        let list = |v: Vec<String>| {
            if v.is_empty() {
                String::new()
            } else {
                format!("elements = {{ {} }}", v.join(", "))
            }
        };
        let blocked = list(blocked.iter().map(Cidr::to_string).collect());
        let guests = list(leases.iter().map(guest_element).collect());
        let dns = list(c.dns.iter().map(Ipv4Addr::to_string).collect());
        let pool = c.pool;
        format!(
            r#"table inet {TABLE}
delete table inet {TABLE}
table inet {TABLE} {{
	comment "Pyo Sandbox networking. The pyo-sandbox daemon owns this table."

	# A tap and the one guest address it may send from.
	set guests {{
		type ifname . ipv4_addr
		{guests}
	}}

	# Never reached from a sandbox.
	set blocked {{
		type ipv4_addr
		flags interval
		{blocked}
	}}

	set dns {{
		type ipv4_addr
		{dns}
	}}

	# Per sandbox egress lists: tap to a chain that drops or returns.
	map egress {{
		type ifname : verdict
	}}

	chain input {{
		type filter hook input priority filter; policy accept;
		iifname "{TAP_PREFIX}*" drop
	}}

	chain forward {{
		type filter hook forward priority filter - 10; policy accept;
		iifname "{TAP_PREFIX}*" jump from_sandbox
		oifname "{TAP_PREFIX}*" ct state established,related accept
		oifname "{TAP_PREFIX}*" drop
	}}

	chain from_sandbox {{
		meta nfproto != ipv4 drop
		iifname . ip saddr != @guests drop
		oifname "{TAP_PREFIX}*" drop
		oifname "tailscale0" drop
		ip daddr @dns meta l4proto {{ tcp, udp }} th dport 53 accept
		ip daddr @blocked drop
		iifname vmap @egress
		accept
	}}

	chain postrouting {{
		type nat hook postrouting priority srcnat; policy accept;
		ip saddr {pool} oifname != "{TAP_PREFIX}*" masquerade
	}}
}}
"#
        )
    }
}

/// The comment on the iptables rules the daemon adds. See "Other
/// firewalls" above.
pub const RULE_COMMENT: &str = "pyo-sandbox";
const DOCKER_USER: &str = "DOCKER-USER";

/// iptables' forward chains, as far as sandboxes care.
#[derive(Debug, Clone, PartialEq)]
pub struct ForwardState {
    /// The `FORWARD` policy is DROP (or anything but ACCEPT).
    pub drops: bool,
    /// Docker's `DOCKER-USER` chain exists.
    pub docker_user: bool,
}

impl ForwardState {
    /// Where the accept rules go, when they are needed.
    pub fn chain(&self) -> Option<&'static str> {
        match (self.drops, self.docker_user) {
            (false, _) => None,
            (true, true) => Some(DOCKER_USER),
            (true, false) => Some("FORWARD"),
        }
    }
}

/// Reads `iptables -S FORWARD`. None when iptables isn't installed or
/// there is no filter table.
pub async fn forward_state() -> Option<ForwardState> {
    let rules = run_out("iptables", &["-w", "-S", "FORWARD"]).await.ok()?;
    let docker_user = run_out("iptables", &["-w", "-S", DOCKER_USER])
        .await
        .is_ok();
    Some(ForwardState {
        drops: forward_drops(&rules),
        docker_user,
    })
}

/// Whether `iptables -S FORWARD` shows a policy that drops.
pub fn forward_drops(rules: &str) -> bool {
    rules
        .lines()
        .find_map(|l| l.strip_prefix("-P FORWARD "))
        .is_some_and(|p| p.trim() != "ACCEPT")
}

/// Whether another firewall's FORWARD policy drops sandbox traffic and
/// iptables isn't there to let it through. Read with nft, so it works
/// without iptables. For the capability report.
pub async fn forward_unhandled() -> Option<String> {
    if forward_state().await.is_some() {
        return None;
    }
    let chain = run_out("nft", &["list", "chain", "ip", "filter", "FORWARD"])
        .await
        .ok()?;
    let drops = chain
        .lines()
        .any(|l| l.contains("hook forward") && l.contains("policy drop"));
    drops.then(|| {
        "The firewall's FORWARD policy is DROP and iptables isn't installed, so sandboxes can't reach the internet. Install iptables.".to_string()
    })
}

/// The tagged rules in `iptables -S <chain>` output, as `-D` arguments.
pub fn tagged_rules(rules: &str) -> Vec<Vec<String>> {
    let tag = format!("--comment {RULE_COMMENT} ");
    rules
        .lines()
        .filter(|l| l.starts_with("-A ") && format!("{l} ").contains(&tag))
        .map(|l| {
            let mut a: Vec<String> = l.split_whitespace().map(str::to_string).collect();
            a[0] = "-D".into();
            a
        })
        .collect()
}

/// The rules that let sandbox traffic past a chain that drops, each as
/// `iptables -I <chain> 1` arguments. Inserted in this order, so the
/// first ends up on top.
pub fn accept_rules(chain: &str) -> Vec<Vec<String>> {
    let tap = format!("{TAP_PREFIX}+");
    let rule = |dir: &str, extra: &[&str]| {
        let mut a: Vec<String> = ["-I", chain, "1", dir, &tap]
            .iter()
            .map(|s| s.to_string())
            .collect();
        a.extend(extra.iter().map(|s| s.to_string()));
        a.extend(
            ["-m", "comment", "--comment", RULE_COMMENT, "-j", "ACCEPT"]
                .iter()
                .map(|s| s.to_string()),
        );
        a
    };
    vec![
        rule(
            "-o",
            &["-m", "conntrack", "--ctstate", "RELATED,ESTABLISHED"],
        ),
        rule("-i", &[]),
    ]
}

/// Removes the daemon's iptables rules, from both chains.
pub async fn remove_forward_rules() {
    for chain in ["FORWARD", DOCKER_USER] {
        let Ok(rules) = run_out("iptables", &["-w", "-S", chain]).await else {
            continue;
        };
        for rule in tagged_rules(&rules) {
            let mut args = vec!["-w"];
            args.extend(rule.iter().map(String::as_str));
            if let Err(e) = run("iptables", &args).await {
                tracing::warn!(error = %format!("{e:#}"), "can't remove an iptables rule");
            }
        }
    }
}

/// Replaces the daemon's iptables rules: none when nothing drops, the
/// accept rules otherwise. Returns the chain they went in.
async fn sync_forward_rules() -> anyhow::Result<Option<&'static str>> {
    remove_forward_rules().await;
    let Some(state) = forward_state().await else {
        if let Some(why) = forward_unhandled().await {
            bail!("{why}");
        }
        return Ok(None);
    };
    let Some(chain) = state.chain() else {
        return Ok(None);
    };
    for rule in accept_rules(chain) {
        let mut args = vec!["-w"];
        args.extend(rule.iter().map(String::as_str));
        run("iptables", &args)
            .await
            .with_context(|| format!("letting sandbox taps through iptables' {chain}"))?;
    }
    Ok(Some(chain))
}

fn guest_element(l: &Lease) -> String {
    format!("\"{}\" . {}", l.tap, l.guest)
}

/// Sorts ranges and drops the ones inside another, so the interval set
/// takes them. Aligned ranges either nest or don't touch.
fn merge(mut v: Vec<Cidr>) -> Vec<Cidr> {
    v.sort_by_key(|c| (c.first(), std::cmp::Reverse(c.last())));
    let mut out: Vec<Cidr> = Vec::new();
    for c in v {
        if out.last().is_some_and(|l| l.overlaps(&c)) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Refuses pools that are too small or clash with special ranges.
pub fn check_pool(pool: &Cidr) -> anyhow::Result<()> {
    if !(8..=29).contains(&pool.prefix) {
        bail!(
            "The sandbox address pool {pool} must be a /8 to a /29, since each sandbox takes a /30."
        );
    }
    for (r, what) in POOL_CLASHES {
        let r: Cidr = r.parse().expect("valid range");
        if pool.overlaps(&r) {
            bail!(
                "The sandbox address pool {pool} overlaps {what} ({r}). Pick another pool in the config."
            );
        }
    }
    Ok(())
}

/// The first route or host address that overlaps the pool, from
/// `ip -j -4 route show table all`. Default routes and the daemon's own
/// taps don't count.
pub fn route_clash(pool: &Cidr, routes_json: &str) -> anyhow::Result<Option<String>> {
    let routes: Vec<serde_json::Value> =
        serde_json::from_str(routes_json).context("reading the host's routes")?;
    for r in routes {
        let dst = r["dst"].as_str().unwrap_or("");
        let dev = r["dev"].as_str().unwrap_or("");
        if dst.is_empty() || dst == "default" || dev.starts_with(TAP_PREFIX) {
            continue;
        }
        let Ok(c) = dst.parse::<Cidr>() else { continue };
        if c.prefix == 0 {
            continue;
        }
        if c.overlaps(pool) {
            return Ok(Some(format!("the route {dst} on {dev}")));
        }
    }
    Ok(None)
}

/// Taps named like the daemon's that no lease owns, from `ip -j link show`.
pub fn stray_taps(links_json: &str, leases: &[Lease]) -> anyhow::Result<Vec<String>> {
    let links: Vec<serde_json::Value> =
        serde_json::from_str(links_json).context("reading the host's links")?;
    Ok(links
        .iter()
        .filter_map(|l| l["ifname"].as_str())
        .filter(|n| {
            n.strip_prefix(TAP_PREFIX)
                .is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|n| !leases.iter().any(|l| l.tap == *n))
        .map(str::to_string)
        .collect())
}

fn tap_exists(tap: &str) -> bool {
    std::path::Path::new("/sys/class/net").join(tap).exists()
}

/// Where `ip netns` keeps named namespaces.
pub const NETNS_DIR: &str = "/var/run/netns";

fn netns_exists(name: &str) -> bool {
    std::path::Path::new(NETNS_DIR).join(name).exists()
}

fn dir_entries(dir: &str) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Namespaces named like the daemon's that no lease owns.
pub fn stray_netns(names: &[String], leases: &[Lease]) -> Vec<String> {
    names
        .iter()
        .filter(|n| {
            n.strip_prefix(TAP_PREFIX)
                .is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|n| !leases.iter().any(|l| &l.tap == *n))
        .cloned()
        .collect()
}

async fn nft(script: &str) -> anyhow::Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting nft")?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("nft: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

async fn run(prog: &str, args: &[&str]) -> anyhow::Result<()> {
    run_out(prog, args).await.map(|_| ())
}

async fn run_out(prog: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("starting {prog}"))?;
    if !out.status.success() {
        bail!(
            "{prog} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> Network {
        Network {
            cfg: NetworkConfig::default(),
            link: LinkKind::Tap,
            tap_uid: 1,
            tap_gid: 1,
            used: Mutex::new(BTreeSet::new()),
        }
    }

    #[test]
    fn cidrs() {
        let c: Cidr = "198.18.7.9/16".parse().unwrap();
        assert_eq!(c.to_string(), "198.18.0.0/16");
        assert!(c.contains(Ipv4Addr::new(198, 18, 255, 255)));
        assert!(!c.contains(Ipv4Addr::new(198, 19, 0, 0)));
        assert!(c.overlaps(&"198.0.0.0/8".parse().unwrap()));
        assert!(!c.overlaps(&"198.19.0.0/16".parse().unwrap()));
        assert_eq!("1.2.3.4".parse::<Cidr>().unwrap().prefix, 32);
        assert!("1.2.3.4/33".parse::<Cidr>().is_err());
        assert!("::1/64".parse::<Cidr>().is_err());
        let j: NetworkConfig = serde_json::from_str(r#"{"pool":"198.19.0.0/20"}"#).unwrap();
        assert_eq!(j.pool, "198.19.0.0/20".parse().unwrap());
        assert_eq!(j.dns.len(), 2);
    }

    #[test]
    fn leases_are_slash_30s() {
        let n = net();
        assert_eq!(n.size(), 16384);
        let l = n.lease_at(3);
        assert_eq!(l.tap, "pyosbx3");
        assert_eq!(l.host, Ipv4Addr::new(198, 18, 0, 13));
        assert_eq!(l.guest, Ipv4Addr::new(198, 18, 0, 14));
        let last = n.lease_at(n.size() - 1);
        assert_eq!(last.guest, Ipv4Addr::new(198, 18, 255, 254));
        assert!(last.tap.len() <= 15);
        let g = n.guest_network(&l);
        assert_eq!((g.iface.as_str(), g.prefix), ("eth0", 30));
        assert_eq!(g.gateway, l.host);
    }

    #[test]
    fn pools_that_clash_are_refused() {
        assert!(check_pool(&"198.18.0.0/16".parse().unwrap()).is_ok());
        for bad in [
            "100.100.0.0/16",
            "10.42.0.0/16",
            "172.17.0.0/16",
            "169.254.0.0/16",
            "198.18.0.0/30",
        ] {
            assert!(check_pool(&bad.parse().unwrap()).is_err(), "{bad}");
        }
        let pool: Cidr = "198.18.0.0/16".parse().unwrap();
        let routes = r#"[
            {"dst":"default","gateway":"10.97.0.1","dev":"eth0"},
            {"dst":"10.97.0.0/24","dev":"eth0"},
            {"dst":"198.18.0.4/30","dev":"pyosbx1"},
            {"type":"local","dst":"10.97.0.249","dev":"eth0"}
        ]"#;
        assert_eq!(route_clash(&pool, routes).unwrap(), None);
        let clash = r#"[{"dst":"198.18.4.0/24","dev":"wg0"}]"#;
        assert!(route_clash(&pool, clash).unwrap().unwrap().contains("wg0"));
    }

    #[test]
    fn strays() {
        let n = net();
        let links = r#"[{"ifname":"lo"},{"ifname":"pyosbx0"},{"ifname":"pyosbx7"},{"ifname":"pyosbxfoo"},{"ifname":"eth0"}]"#;
        assert_eq!(
            stray_taps(links, &[n.lease_at(0)]).unwrap(),
            vec!["pyosbx7".to_string()]
        );
        let names: Vec<String> = ["pyosbx0", "pyosbx3", "pyosbxx", "cni-1"]
            .map(String::from)
            .to_vec();
        assert_eq!(stray_netns(&names, &[n.lease_at(0)]), vec!["pyosbx3"]);
    }

    #[test]
    fn gvisor_leases_get_a_namespace() {
        let mut n = net();
        assert_eq!(n.netns_path(&n.lease_at(4)), None, "taps have none");
        n.link = LinkKind::Veth;
        assert_eq!(
            n.netns_path(&n.lease_at(4)).as_deref(),
            Some("/var/run/netns/pyosbx4")
        );
    }

    #[test]
    fn the_ruleset_blocks_what_it_must() {
        let n = net();
        let r = n.ruleset(&[n.lease_at(0), n.lease_at(2)]);
        // Atomic replace: make, delete, make.
        assert!(r.starts_with("table inet pyo_sandbox\ndelete table inet pyo_sandbox\n"));
        assert!(r.contains(r#"elements = { "pyosbx0" . 198.18.0.2, "pyosbx2" . 198.18.0.10 }"#));
        for must in [
            "100.64.0.0/10",
            "169.254.0.0/16",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "198.18.0.0/15",
        ] {
            assert!(r.contains(must), "{must}");
        }
        // The pool sits inside 198.18.0.0/15, so the set holds it once.
        assert!(!r.contains("198.18.0.0/16,"));
        assert!(r.contains(r#"oifname "tailscale0" drop"#));
        // Before iptables' FORWARD, so these drops come first.
        assert!(r.contains("hook forward priority filter - 10;"));
        assert!(r.contains(r#"iifname "pyosbx*" drop"#));
        assert!(r.contains("iifname . ip saddr != @guests drop"));
        assert!(r.contains("iifname vmap @egress"));
        assert!(r.contains(r#"ip saddr 198.18.0.0/16 oifname != "pyosbx*" masquerade"#));
        assert!(r.contains("elements = { 1.1.1.1, 8.8.8.8 }"));
        // No leases: an empty set, not an empty element list.
        assert!(!n.ruleset(&[]).contains("elements = {  }"));
    }

    #[test]
    fn forward_rules() {
        assert!(forward_drops(
            "-P FORWARD DROP\n-A FORWARD -j DOCKER-USER\n"
        ));
        assert!(!forward_drops("-P FORWARD ACCEPT\n"));
        assert!(!forward_drops(""));
        let s = ForwardState {
            drops: true,
            docker_user: true,
        };
        assert_eq!(s.chain(), Some("DOCKER-USER"));
        let s = ForwardState {
            drops: true,
            docker_user: false,
        };
        assert_eq!(s.chain(), Some("FORWARD"));
        let s = ForwardState {
            drops: false,
            docker_user: true,
        };
        assert_eq!(s.chain(), None);
        let r = accept_rules("FORWARD");
        assert_eq!(
            r[0].join(" "),
            "-I FORWARD 1 -o pyosbx+ -m conntrack --ctstate RELATED,ESTABLISHED -m comment --comment pyo-sandbox -j ACCEPT"
        );
        assert_eq!(
            r[1].join(" "),
            "-I FORWARD 1 -i pyosbx+ -m comment --comment pyo-sandbox -j ACCEPT"
        );
        let listed = "-N DOCKER-USER\n\
-A DOCKER-USER -i pyosbx+ -m comment --comment pyo-sandbox -j ACCEPT\n\
-A DOCKER-USER -o pyosbx+ -m conntrack --ctstate RELATED,ESTABLISHED -m comment --comment pyo-sandbox -j ACCEPT\n\
-A DOCKER-USER -m comment --comment pyo-sandbox-other -j ACCEPT\n\
-A DOCKER-USER -j RETURN\n";
        let t = tagged_rules(listed);
        assert_eq!(t.len(), 2);
        assert_eq!(
            t[0].join(" "),
            "-D DOCKER-USER -i pyosbx+ -m comment --comment pyo-sandbox -j ACCEPT"
        );
    }

    #[test]
    fn merging_ranges() {
        let v: Vec<Cidr> = [
            "10.0.0.0/8",
            "10.1.0.0/16",
            "1.0.0.0/8",
            "198.18.0.0/16",
            "198.18.0.0/15",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        let m: Vec<String> = merge(v).iter().map(Cidr::to_string).collect();
        assert_eq!(m, vec!["1.0.0.0/8", "10.0.0.0/8", "198.18.0.0/15"]);
    }
}
