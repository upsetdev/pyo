//! Sandbox networking with real Firecracker microVMs, taps and nftables.
//! Runs only with `PYO_SANDBOX_NET_E2E=1` as root, so plain `cargo test`
//! skips it. It changes the host's network (taps, the `pyo_sandbox` table,
//! a network namespace and IPv4 forwarding), so run it in a throwaway VM,
//! never on a laptop or a real server:
//!
//! ```text
//! crates/pyo-sandbox/scripts/e2e-assets.sh      # Firecracker, the jailer and a kernel
//! cargo build --release -p pyo-envd --target x86_64-unknown-linux-musl
//! cargo test -p pyo-sandbox --test network_e2e --target x86_64-unknown-linux-musl --no-run
//! # copy the test binary, envd, Firecracker, the jailer and the kernel into the VM, then as root:
//! PYO_SANDBOX_NET_E2E=1 PYO_SANDBOX_FIRECRACKER=... PYO_SANDBOX_JAILER=... \
//!   PYO_SANDBOX_KERNEL=... PYO_SANDBOX_ENVD=... ./network_e2e-<hash> --nocapture
//! ```
//!
//! With `PYO_SANDBOX_JAILER` set, VMs run under the jailer as uid 65534,
//! like on a real server. The template is the host's static busybox plus
//! envd (`apt install busybox-static`).
//!
//! What it checks: sandboxes reach the internet and DNS, and never the
//! host, cloud metadata, RFC1918, the tailnet (a namespace behind a
//! `tailscale0` veth stands in for it) or each other. "No network" has no
//! interface. Template snapshots with an interface restore into new taps
//! and addresses. Pause and resume, a daemon restart and delete keep the
//! taps and the table in step.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;

use pyo_proto::sandbox::wire::Cmd;
use pyo_proto::sandbox::{
    ExecRequest, ExecResult, NetworkMode, Runtime, SandboxSpec, sandbox_id_from,
};
use pyo_sandbox::config::Config;
use pyo_sandbox::detect::{self, Inputs};
use pyo_sandbox::firecracker::{Firecracker, Jailer};
use pyo_sandbox::manager::Manager;
use pyo_sandbox::network::{
    LinkKind, Network, NetworkConfig, RULE_COMMENT, TABLE, TAP_PREFIX, forward_drops, tagged_rules,
};
use pyo_sandbox::storage::{self, Layout};

const TEAM: &str = "team-net";
/// Stands in for the tailnet: a namespace behind a veth named
/// `tailscale0`, with a tailnet address and a public-looking subnet route.
const NS: &str = "pyo-net-e2e";
const TS_HOST: &str = "100.100.0.1";
const TS_PEER: &str = "100.100.0.2";
/// A subnet routed over the "tailnet", so only the `tailscale0` rule
/// stops it.
const TS_ROUTED: &str = "44.31.0.2";
/// A private LAN next to the host (RFC1918), on its own veth in the same
/// namespace, so the `blocked` set has something real to stop.
const LAN_HOST: &str = "10.254.0.1";
const LAN_PEER: &str = "10.254.0.2";
const LISTEN_PORT: u16 = 8080;

fn enabled() -> bool {
    if std::env::var("PYO_SANDBOX_NET_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set PYO_SANDBOX_NET_E2E=1, as root in a throwaway VM");
        return false;
    }
    if !pyo_sandbox::config::is_root() {
        eprintln!("skipping: the network e2e needs root");
        return false;
    }
    true
}

fn env_path(var: &str) -> PathBuf {
    let p = PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} isn't set")));
    assert!(p.exists(), "{var}: {} is missing", p.display());
    p
}

fn sh(cmd: &str) -> (bool, String) {
    let out = Command::new("sh").args(["-c", cmd]).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn must(cmd: &str) -> String {
    let (ok, out) = sh(cmd);
    assert!(ok, "{cmd}: {out}");
    out
}

fn taps() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/class/net")
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(TAP_PREFIX))
        .collect();
    v.sort();
    v
}

/// The daemon's tagged iptables rules, per chain.
fn tagged() -> Vec<(String, usize)> {
    ["FORWARD", "DOCKER-USER"]
        .iter()
        .map(|c| {
            let (_, rules) = sh(&format!("iptables -w -S {c} 2>/dev/null"));
            (c.to_string(), tagged_rules(&rules).len())
        })
        .collect()
}

/// iptables' FORWARD policy line, or "" without iptables.
fn forward_policy() -> String {
    let (_, rules) = sh("iptables -w -S FORWARD 2>/dev/null");
    rules
        .lines()
        .find(|l| l.starts_with("-P FORWARD"))
        .unwrap_or("")
        .to_string()
}

fn guests_set() -> String {
    sh(&format!("nft list set inet {TABLE} guests")).1
}

/// The host's own address on its default route.
fn host_ip() -> String {
    let out = must("ip -4 -o route get 1.1.1.1");
    out.split_whitespace()
        .skip_while(|w| *w != "src")
        .nth(1)
        .expect("a src address")
        .to_string()
}

fn host_gateway() -> String {
    let out = must("ip -4 -o route show default");
    out.split_whitespace()
        .skip_while(|w| *w != "via")
        .nth(1)
        .expect("a default gateway")
        .to_string()
}

/// A template root: the host's static busybox, envd, and the host's curl
/// with its libraries and CA bundle. busybox wget's own TLS fails inside
/// the guest (plain HTTP comes through byte for byte), and curl is the
/// real test anyway.
async fn busybox_template(layout: &Layout, envd: &Path) {
    let dir = layout.root.join("template-src");
    let _ = std::fs::remove_dir_all(&dir);
    for d in [
        "bin", "sbin", "proc", "sys", "dev", "tmp", "run", "root", "mnt", "etc",
    ] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::copy("/usr/bin/busybox", dir.join("bin/busybox")).unwrap();
    let list = Command::new("/usr/bin/busybox")
        .arg("--list")
        .output()
        .unwrap();
    for applet in String::from_utf8_lossy(&list.stdout).lines() {
        let p = dir.join("bin").join(applet);
        if applet != "busybox" && !p.exists() {
            std::os::unix::fs::symlink("busybox", p).unwrap();
        }
    }
    std::fs::copy(envd, dir.join("sbin/pyo-envd")).unwrap();
    std::fs::write(dir.join("etc/passwd"), "root:x:0:0:root:/root:/bin/sh\n").unwrap();
    let curl = Path::new("/usr/bin/curl");
    assert!(curl.exists(), "the test needs curl on the host");
    let ldd = must("ldd /usr/bin/curl");
    let mut files: Vec<String> = ldd
        .split_whitespace()
        .filter(|w| w.starts_with('/'))
        .map(str::to_string)
        .collect();
    files.push("/usr/bin/curl".into());
    files.push("/etc/ssl/certs/ca-certificates.crt".into());
    for f in files {
        let to = dir.join(f.trim_start_matches('/'));
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        // Follows symlinks, so the library lands under its linked name.
        std::fs::copy(&f, &to).unwrap();
    }
    let out = layout.template_rootfs("busybox");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    storage::ext4_from_dir(&dir, &out, 128, "busybox")
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The fake tailnet, and a listener in it.
struct FakeTailnet(Child);

impl FakeTailnet {
    fn up() -> FakeTailnet {
        Self::down();
        must(&format!(
            "ip netns add {NS} && ip link add tailscale0 type veth peer name ts-peer \
             && ip link set ts-peer netns {NS} \
             && ip addr add {TS_HOST}/24 dev tailscale0 && ip link set tailscale0 up \
             && ip -n {NS} addr add {TS_PEER}/24 dev ts-peer \
             && ip -n {NS} addr add {TS_ROUTED}/24 dev ts-peer \
             && ip -n {NS} link set ts-peer up && ip -n {NS} link set lo up \
             && ip -n {NS} route add default via {TS_HOST} \
             && ip route add 44.31.0.0/24 dev tailscale0 \
             && ip link add pyo-e2e-lan type veth peer name lan-peer \
             && ip link set lan-peer netns {NS} \
             && ip addr add {LAN_HOST}/24 dev pyo-e2e-lan && ip link set pyo-e2e-lan up \
             && ip -n {NS} addr add {LAN_PEER}/24 dev lan-peer && ip -n {NS} link set lan-peer up"
        ));
        let child = Command::new("ip")
            .args(["netns", "exec", NS, "busybox", "nc", "-ll", "-p"])
            .arg(LISTEN_PORT.to_string())
            .args(["-e", "echo", "hello"])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        FakeTailnet(child)
    }

    fn down() {
        let _ = sh(
            "ip link del tailscale0 2>/dev/null; ip link del pyo-e2e-lan 2>/dev/null; ip netns del pyo-net-e2e 2>/dev/null; ip route del 44.31.0.0/24 2>/dev/null",
        );
    }
}

impl Drop for FakeTailnet {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        Self::down();
    }
}

/// Kills this run's VMs and removes its files, taps and table, also when
/// the test fails.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = sh(&format!(
            "pkill -9 -f 'firecracker.*{}' ; pkill -9 -f 'jailer.*{}'",
            self.0.display(),
            self.0.display()
        ));
        let _ = sh("for p in $(pgrep -x firecracker); do kill -9 $p; done");
        for t in taps() {
            let _ = sh(&format!("ip link del {t}"));
        }
        let _ = sh(&format!("nft delete table inet {TABLE}"));
        let _ = sh(&format!(
            "for c in FORWARD DOCKER-USER; do iptables -w -S $c 2>/dev/null | grep -e '--comment {RULE_COMMENT} ' | sed 's/^-A /-D /' | while read -r r; do iptables -w $r; done; done"
        ));
        let _ = sh("ip link del pyosbx999 2>/dev/null");
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Env {
    cfg: Config,
    layout: Layout,
    fc: Firecracker,
    envd: PathBuf,
}

impl Env {
    /// What the daemon does on start: detect, open the network, adopt.
    async fn daemon(&self) -> Arc<Manager> {
        let mut cap = detect::detect(Inputs {
            fc: &self.fc,
            layout: &self.layout,
            envd: &self.envd,
            runsc: &self.cfg.runsc,
        })
        .await;
        assert_eq!(cap.runtime, Runtime::Firecracker, "{}", cap.summary());
        let (uid, gid) = match &self.fc.jailer {
            Some(j) => (j.uid, j.gid),
            None => (0, 0),
        };
        let net = Network::open(self.cfg.network.clone(), LinkKind::Tap, uid, gid)
            .await
            .expect("networking opens");
        cap.network = false;
        let mgr = Manager::new(self.cfg.clone(), self.layout.clone(), self.fc.clone(), cap)
            .with_network(Some(net));
        mgr.adopt().await;
        assert!(mgr.capability().network, "networking is on after adopt");
        Arc::new(mgr)
    }
}

fn spec(n: u8, network: NetworkMode) -> SandboxSpec {
    let mut s = SandboxSpec::new(sandbox_id_from([n; 16]), TEAM, "busybox");
    s.memory_mib = 256;
    s.disk_mib = 256;
    s.network = network;
    s
}

async fn exec(mgr: &Manager, id: &str, cmd: &str) -> ExecResult {
    mgr.exec(
        id,
        &ExecRequest {
            cmd: Cmd::Shell(cmd.into()),
            cwd: None,
            env: BTreeMap::new(),
            stdin: None,
            timeout_ms: Some(30_000),
        },
    )
    .await
    .unwrap()
}

/// Whether a TCP connection from the sandbox to `addr port` opens.
async fn connects(mgr: &Manager, id: &str, addr: &str, port: u16) -> bool {
    let r = exec(mgr, id, &format!("nc -w 3 {addr} {port} </dev/null")).await;
    r.exit_code == Some(0)
}

/// Whether the host itself pings out. Some clouds (Azure, so GitHub's
/// runners) drop outbound ICMP, and then a sandbox can't either.
fn host_pings() -> bool {
    static PINGS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PINGS.get_or_init(|| {
        let ok = sh("ping -c 1 -W 5 1.1.1.1 >/dev/null").0;
        println!("net-e2e host pings 1.1.1.1: {ok}");
        ok
    })
}

/// Internet: `curl https://1.1.1.1` with the certificate checked, ICMP
/// (when the host pings out), DNS, and HTTPS by name.
async fn assert_internet(mgr: &Manager, id: &str) {
    let r = exec(
        mgr,
        id,
        "curl -sS -m 10 -o /dev/null https://1.1.1.1/ && echo https-ok; \
         ping -c 1 -W 5 1.1.1.1 >/dev/null && echo ping-ok; \
         nslookup one.one.one.one >/dev/null && echo dns-ok; \
         curl -sS -m 10 -o /dev/null https://example.com/ && echo https-name-ok",
    )
    .await;
    println!(
        "net-e2e internet {id}: {}",
        r.stdout.trim().replace('\n', " ")
    );
    for want in ["https-ok", "ping-ok", "dns-ok", "https-name-ok"] {
        if want == "ping-ok" && !host_pings() {
            continue;
        }
        assert!(r.stdout.contains(want), "{id}: {want} missing: {r:?}");
    }
}

async fn guest_ip(mgr: &Manager, id: &str) -> String {
    let r = exec(mgr, id, "ip -4 addr show eth0").await;
    r.stdout
        .split_whitespace()
        .skip_while(|w| *w != "inet")
        .nth(1)
        .unwrap_or_else(|| panic!("no eth0 address: {r:?}"))
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_networking() {
    if !enabled() {
        return;
    }
    let fc_bin = env_path("PYO_SANDBOX_FIRECRACKER");
    let kernel = env_path("PYO_SANDBOX_KERNEL");
    let envd = env_path("PYO_SANDBOX_ENVD");
    let jailer = std::env::var_os("PYO_SANDBOX_JAILER").map(PathBuf::from);
    let root = std::env::var_os("PYO_SANDBOX_NET_ROOT")
        .map(PathBuf::from)
        .unwrap_or("/var/tmp/pyo-sandbox-net-e2e".into());
    let _ = std::fs::remove_dir_all(&root);
    let _cleanup = Cleanup(root.clone());
    let jailed = jailer.is_some();
    println!("net-e2e jailer: {jailed}");
    let cfg = Config {
        server_id: "srv-net".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        root: root.clone(),
        firecracker: fc_bin.clone(),
        jailer: jailer.clone(),
        kernel: kernel.clone(),
        envd: envd.clone(),
        runsc: "runsc".into(),
        keys: vec![],
        host_reserve_mib: Some(512),
        memory_overcommit: 1.0,
        cpu_template: None,
        jail_uid: 65534,
        jail_gid: 65534,
        cgroup_parent: "pyo-sandbox-e2e".into(),
        network: NetworkConfig::default(),
        edge: None,
        dev: !jailed,
    };
    let layout = Layout::new(&root, jailed);
    layout.create().await.unwrap();
    let fc = Firecracker {
        firecracker: fc_bin,
        kernel,
        jailer: jailer.map(|j| Jailer {
            jailer: j,
            uid: cfg.jail_uid,
            gid: cfg.jail_gid,
            chroot_base: root.join("jail"),
            cgroup_parent: cfg.cgroup_parent.clone(),
        }),
        cpu_template: None,
        serial: false,
    };
    let env = Env {
        cfg,
        layout: layout.clone(),
        fc,
        envd: envd.clone(),
    };
    busybox_template(&layout, &envd).await;
    let _tailnet = FakeTailnet::up();
    // The fake tailnet answers the host, so a refused sandbox means the
    // rules refused it.
    assert!(
        must(&format!(
            "busybox nc -w 3 {TS_PEER} {LISTEN_PORT} </dev/null"
        ))
        .contains("hello")
    );
    assert!(
        must(&format!(
            "busybox nc -w 3 {TS_ROUTED} {LISTEN_PORT} </dev/null"
        ))
        .contains("hello")
    );
    // A stray tap from an earlier run goes on start.
    must("ip tuntap add dev pyosbx999 mode tap");

    let policy = forward_policy();
    let (_, docker_user) = sh("iptables -w -S DOCKER-USER");
    let docker_user = docker_user.starts_with("-N DOCKER-USER");
    let drops = forward_drops(&policy);
    println!("net-e2e forward policy: {policy:?}, DOCKER-USER: {docker_user}");
    let mgr = env.daemon().await;
    assert!(!taps().contains(&"pyosbx999".to_string()), "{:?}", taps());
    let table = must(&format!("nft list table inet {TABLE}"));
    println!("net-e2e table: {table}");
    // Ahead of iptables' FORWARD, so this table's drops come first.
    assert!(table.contains("priority filter - 10;"), "{table}");
    // Another firewall that drops forwarded traffic gets two narrow accept
    // rules, in DOCKER-USER when Docker made it.
    let want_chain = if docker_user {
        "DOCKER-USER"
    } else {
        "FORWARD"
    };
    let expect: Vec<(String, usize)> = ["FORWARD", "DOCKER-USER"]
        .iter()
        .map(|c| (c.to_string(), if drops && *c == want_chain { 2 } else { 0 }))
        .collect();
    println!("net-e2e iptables rules: {:?}", tagged());
    assert_eq!(tagged(), expect, "the daemon's iptables rules");

    // ── Cold boot with network ──
    let a = spec(1, NetworkMode::Internet);
    let st = mgr.create(a.clone()).await.unwrap();
    println!("net-e2e cold start with network: {:?} ms", st.start_ms);
    let ip_a = guest_ip(&mgr, &a.id).await;
    println!("net-e2e a: {ip_a}");
    assert_eq!(ip_a, "198.18.0.2/30");
    let resolv = exec(&mgr, &a.id, "cat /etc/resolv.conf; ip route").await;
    println!("net-e2e a resolv and routes: {}", resolv.stdout.trim());
    assert!(resolv.stdout.contains("nameserver 1.1.1.1"));
    assert!(resolv.stdout.contains("default via 198.18.0.1"));
    assert_internet(&mgr, &a.id).await;
    if jailed {
        // The jailer dropped Firecracker to the jail user, which owns the tap.
        let st = must("for p in $(pgrep -x firecracker); do grep '^Uid' /proc/$p/status; done");
        println!("net-e2e jailed firecracker: {}", st.trim());
        assert!(st.contains("65534") && !st.contains("\t0\t"), "{st}");
        let owner = std::fs::read_to_string("/sys/class/net/pyosbx0/owner").unwrap();
        assert_eq!(owner.trim(), "65534");
    }

    // ── Always blocked ──
    let host = host_ip();
    let gw = host_gateway();
    // The host answers on SSH. A sandbox must not.
    assert!(must(&format!("busybox nc -w 3 {host} 22 </dev/null; echo rc=$?")).contains("SSH"));
    assert!(
        must(&format!(
            "busybox nc -w 3 {LAN_PEER} {LISTEN_PORT} </dev/null"
        ))
        .contains("hello"),
        "the host reaches the fake LAN"
    );
    // A real gateway may not answer on 53 (Azure's doesn't), and then
    // there's nothing to check there.
    let gw_answers = sh(&format!("busybox nc -w 3 {gw} 53 </dev/null")).0;
    println!("net-e2e the host reaches its gateway {gw}:53: {gw_answers}");
    let mut blocked = vec![
        ("the host", host.as_str(), 22),
        ("a LAN next to the host (RFC1918)", LAN_PEER, LISTEN_PORT),
        ("cloud metadata", "169.254.169.254", 80),
        ("the tailnet", TS_PEER, LISTEN_PORT),
        ("a subnet behind tailscale0", TS_ROUTED, LISTEN_PORT),
        ("the gateway on the tap", "198.18.0.1", 22),
    ];
    if gw_answers {
        blocked.push(("the host's gateway", gw.as_str(), 53));
    }
    for (what, addr, port) in blocked {
        let ok = connects(&mgr, &a.id, addr, port).await;
        println!("net-e2e blocked {what} {addr}:{port}: {}", !ok);
        assert!(!ok, "a sandbox reached {what} at {addr}:{port}");
    }
    // A spoofed source address gets nowhere.
    let r = exec(
        &mgr,
        &a.id,
        "ip addr add 198.18.0.6/30 dev eth0 && ping -c 1 -W 3 -I 198.18.0.6 1.1.1.1 >/dev/null; echo rc=$?; ip addr del 198.18.0.6/30 dev eth0",
    )
    .await;
    println!("net-e2e spoof: {}", r.stdout.trim());
    assert!(r.stdout.contains("rc=1"), "{r:?}");

    // ── Another sandbox ──
    let b = spec(2, NetworkMode::Internet);
    mgr.create(b.clone()).await.unwrap();
    let ip_b = guest_ip(&mgr, &b.id).await;
    assert_eq!(ip_b, "198.18.0.6/30");
    exec(
        &mgr,
        &b.id,
        "nc -ll -p 8080 -e echo hi-from-b >/dev/null 2>&1 &",
    )
    .await;
    assert!(
        exec(&mgr, &b.id, "nc -w 3 127.0.0.1 8080 </dev/null")
            .await
            .stdout
            .contains("hi-from-b"),
        "b's listener is up"
    );
    let ok = connects(&mgr, &a.id, "198.18.0.6", 8080).await;
    println!("net-e2e blocked another sandbox: {}", !ok);
    assert!(!ok, "a reached b");
    // Nor does the host open connections into a sandbox.
    let (host_to_b, _) = sh("busybox nc -w 3 198.18.0.6 8080 </dev/null | grep -q hi-from-b");
    assert!(!host_to_b, "the host reached b");

    // ── No network ──
    let taps_before = taps();
    let c = spec(3, NetworkMode::None);
    mgr.create(c.clone()).await.unwrap();
    let r = exec(&mgr, &c.id, "ls /sys/class/net").await;
    println!("net-e2e no network: links {:?}", r.stdout.trim());
    assert_eq!(r.stdout.trim(), "lo");
    assert_eq!(taps(), taps_before, "no tap for a sandbox without network");
    assert!(!connects(&mgr, &c.id, "1.1.1.1", 443).await);

    // ── Template snapshot with network, restored into new taps ──
    let (key, ms) = mgr
        .snapshot_template("busybox", 1, 256, 256, Some(NetworkMode::Internet))
        .await
        .unwrap();
    println!("net-e2e template snapshot with network: {key} in {ms} ms");
    assert!(key.contains("-net-"), "{key}");
    let (key_off, _) = mgr
        .snapshot_template("busybox", 1, 256, 256, Some(NetworkMode::None))
        .await
        .unwrap();
    assert!(!key_off.contains("-net-"), "{key_off}");
    assert_eq!(taps().len(), 2, "the build VM's tap is gone: {:?}", taps());
    let mut restored = Vec::new();
    for n in 10..13u8 {
        let s = spec(n, NetworkMode::Internet);
        let st = mgr.create(s.clone()).await.unwrap();
        assert!(st.from_snapshot, "restored from the network snapshot");
        println!(
            "net-e2e restore with network {}: start {:?} ms, {}",
            s.id,
            st.start_ms,
            guest_ip(&mgr, &s.id).await
        );
        restored.push(s);
    }
    let ips: std::collections::BTreeSet<String> = {
        let mut v = std::collections::BTreeSet::new();
        for s in &restored {
            v.insert(guest_ip(&mgr, &s.id).await);
        }
        v
    };
    assert_eq!(ips.len(), 3, "each restore has its own address: {ips:?}");
    assert_internet(&mgr, &restored[0].id).await;
    assert_internet(&mgr, &restored[2].id).await;
    assert!(!connects(&mgr, &restored[1].id, TS_PEER, LISTEN_PORT).await);
    assert!(!connects(&mgr, &restored[1].id, "198.18.0.2", 22).await);
    let off = spec(13, NetworkMode::None);
    let st = mgr.create(off.clone()).await.unwrap();
    assert!(st.from_snapshot);
    assert_eq!(
        exec(&mgr, &off.id, "ls /sys/class/net").await.stdout.trim(),
        "lo"
    );

    // ── Pause and resume keep the tap and address ──
    let p = &restored[0];
    let ip_before = guest_ip(&mgr, &p.id).await;
    mgr.pause(&p.id).await.unwrap();
    let tap_kept = taps().len();
    mgr.resume(&p.id).await.unwrap();
    assert_eq!(taps().len(), tap_kept);
    assert_eq!(guest_ip(&mgr, &p.id).await, ip_before);
    assert_internet(&mgr, &p.id).await;
    println!("net-e2e pause and resume: ok, {ip_before}");

    // ── Daemon restart: adopt, rebuild the table, drop strays ──
    let ids: Vec<String> = mgr.list().iter().map(|s| s.id.clone()).collect();
    let taps_running = taps();
    // A paused sandbox loses its tap on the restart, and gets it back.
    let paused = &restored[1];
    mgr.pause(&paused.id).await.unwrap();
    drop(mgr);
    let paused_lease =
        std::fs::read_to_string(layout.vm_dir(&paused.id).join("sandbox.json")).unwrap();
    let paused_tap =
        serde_json::from_str::<serde_json::Value>(&paused_lease).unwrap()["lease"]["tap"]
            .as_str()
            .unwrap()
            .to_string();
    must(&format!("ip link del {paused_tap}"));
    must("ip tuntap add dev pyosbx999 mode tap");
    // A wiped table comes back on start.
    must(&format!("nft delete table inet {TABLE}"));
    let mgr = env.daemon().await;
    let mut after: Vec<String> = mgr.list().iter().map(|s| s.id.clone()).collect();
    let mut before = ids.clone();
    after.sort();
    before.sort();
    assert_eq!(after, before, "every sandbox is adopted");
    assert_eq!(taps(), taps_running, "the same taps, the stray one gone");
    let set = guests_set();
    for s in [&a, &b, &restored[0]] {
        let lease_ip = guest_ip(&mgr, &s.id).await;
        let ip = lease_ip.trim_end_matches("/30");
        assert!(set.contains(ip), "{ip} isn't in {set}");
    }
    assert_eq!(
        tagged(),
        expect,
        "the iptables rules are replaced, not added twice"
    );
    assert_internet(&mgr, &a.id).await;
    assert!(!connects(&mgr, &a.id, TS_PEER, LISTEN_PORT).await);
    assert!(!connects(&mgr, &a.id, "169.254.169.254", 80).await);
    mgr.resume(&paused.id).await.unwrap();
    assert_internet(&mgr, &paused.id).await;
    println!("net-e2e restart adoption: ok, {} sandboxes", after.len());

    // ── A lease is reused after delete, and delete cleans up ──
    mgr.delete(&b.id).await.unwrap();
    assert!(!guests_set().contains("198.18.0.6"));
    let d = spec(20, NetworkMode::Internet);
    mgr.create(d.clone()).await.unwrap();
    assert_eq!(guest_ip(&mgr, &d.id).await, "198.18.0.6/30");
    assert_internet(&mgr, &d.id).await;
    mgr.delete_all().await;
    assert!(taps().is_empty(), "taps left: {:?}", taps());
    let set = guests_set();
    assert!(!set.contains("198.18."), "guests left: {set}");
    assert_eq!(
        std::fs::read_dir(layout.sandboxes()).unwrap().count(),
        0,
        "VM directories left"
    );
    assert_eq!(forward_policy(), policy, "the FORWARD policy is untouched");
    println!("net-e2e cleanup after delete: ok");
}
