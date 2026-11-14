//! The one-line commands Pyo shows for setting up servers. They run
//! generic scripts (assets/, served by every API under /install/ and
//! published at pyo.dev) with the details as arguments, so the scripts hold
//! no secrets:
//! - connect.sh makes a machine a new cluster (k3s and the agent);
//! - join.sh adds a server to an existing cluster (or a build-only server).

/// The scripts, served at /install/join.sh and /install/connect.sh.
pub const SCRIPT: &str = include_str!("../assets/join.sh");
pub const CONNECT_SCRIPT: &str = include_str!("../assets/connect.sh");

/// Where the scripts are published, for when this Pyo has no address
/// another machine can reach.
pub const PUBLISHED_SCRIPTS_URL: &str = "https://pyo.dev";

/// Where the commands for other machines fetch their scripts from, first
/// of:
/// 1. `PYO_SCRIPTS_URL`, when set (a mirror, or pyo.dev);
/// 2. Pyo itself (`<url>/install`) at `reach`: the address the command gives
///    the machine for Pyo (the tailnet API, or the public URL people gave),
///    which it has to reach anyway;
/// 3. Pyo on the tailnet (`PYO_TAILNET_API_URL`, then its HTTPS dashboard);
/// 4. pyo.dev, when Pyo is only on its own machine or in its cluster.
pub fn scripts_url(reach: Option<&str>) -> String {
    let configured = std::env::var("PYO_SCRIPTS_URL").ok();
    let tailnet = std::env::var("PYO_TAILNET_API_URL")
        .ok()
        .map(|u| friendly(&u));
    pick_scripts_url(
        configured.as_deref(),
        &[reach, tailnet.as_deref(), crate::web::public_url()],
    )
}

fn pick_scripts_url(configured: Option<&str>, pyo: &[Option<&str>]) -> String {
    if let Some(u) = configured.map(str::trim).filter(|u| !u.is_empty()) {
        return u.trim_end_matches('/').to_owned();
    }
    match pyo
        .iter()
        .flatten()
        .map(|u| u.trim().trim_end_matches('/'))
        .find(|u| outside(u) && secure_transport(u))
    {
        Some(u) => format!("{u}/install"),
        None => PUBLISHED_SCRIPTS_URL.to_owned(),
    }
}

/// The host of an http(s) URL, lowercase, without brackets or port.
fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        authority.split(':').next().unwrap_or("")
    };
    Some(host.to_ascii_lowercase())
}

/// Whether a URL is fit to carry the agent's token and the secrets in its
/// commands: https, or plain http only to a private address (this machine,
/// a private network, the tailnet, the cluster), where it never crosses the
/// internet in the clear.
pub fn secure_transport(url: &str) -> bool {
    let url = url.trim();
    if url.starts_with("https://") {
        return url_host(url).is_some_and(|h| !h.is_empty());
    }
    if !url.starts_with("http://") {
        return false;
    }
    let Some(host) = url_host(url).filter(|h| !h.is_empty()) else {
        return false;
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v)) => {
            let [a, b, ..] = v.octets();
            v.is_loopback() || v.is_private() || (a == 100 && (64..=127).contains(&b))
        }
        Ok(std::net::IpAddr::V6(v)) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00,
        Err(_) => {
            let name = host.trim_end_matches('.');
            name == "localhost"
                || name.ends_with(".localhost")
                || name.ends_with(".svc")
                || name.contains(".svc.")
                || name.ends_with(".cluster.local")
                // Tailscale's MagicDNS names: tailnet addresses.
                || name.ends_with(".ts.net")
        }
    }
}

/// Pyo's tailnet IP: the host of `PYO_TAILNET_API_URL` ("http://100.x:8000").
fn pyo_tailnet_ip() -> Option<String> {
    let url = std::env::var("PYO_TAILNET_API_URL").ok()?;
    url_host(url.trim()).filter(|h| h.parse::<std::net::Ipv4Addr>().is_ok())
}

/// An address in a command people copy (a URL, "host:port" or a host),
/// with Pyo's tailnet IP as its tailnet name ("<host>.<tailnet>.ts.net")
/// when it has one. The scripts look the name up in Tailscale's own list of
/// the tailnet's machines (assets/connect.sh, join.sh: `tailnet_host`), so
/// it works without MagicDNS on the machine, and what the machine keeps
/// (k3s's server, the agent's address, the registry) is the IP. Without a
/// name, or for any other address, as it is.
pub fn friendly(addr: &str) -> String {
    with_tailnet_name(
        addr,
        pyo_tailnet_ip().as_deref(),
        crate::web::tailnet_name(),
    )
}

fn with_tailnet_name(addr: &str, ip: Option<&str>, name: Option<&str>) -> String {
    let (Some(ip), Some(name)) = (ip, name) else {
        return addr.to_owned();
    };
    let (scheme, rest) = addr
        .split_once("://")
        .map_or(("", addr), |(s, r)| (&addr[..s.len() + 3], r));
    let end = rest.find([':', '/']).unwrap_or(rest.len());
    if &rest[..end] == ip {
        format!("{scheme}{name}{}", &rest[end..])
    } else {
        addr.to_owned()
    }
}

/// An http(s) URL another machine can reach: not this machine's loopback,
/// nor an in-cluster Service.
fn outside(url: &str) -> bool {
    let Some(host) = url_host(url) else {
        return false;
    };
    !(host.is_empty()
        || host.contains(char::is_whitespace)
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.starts_with("127.")
        || host == "::1"
        || host == "0.0.0.0"
        || host.ends_with(".svc")
        || host.contains(".svc.")
        || host.ends_with(".cluster.local"))
}

/// Where "This machine"'s command fetches connect.sh: from Pyo on this
/// machine, so connecting it never needs the internet (nor PYO_SCRIPTS_URL).
pub fn local_scripts_url() -> String {
    format!("{}/install", local_api_url().trim_end_matches('/'))
}

pub struct Join<'a> {
    /// The control plane's tailnet address (its name when it's Pyo's
    /// machine and the tailnet has MagicDNS, else its IP).
    pub server_ip: &'a str,
    /// `<id>.<secret>`, valid an hour.
    pub token: &'a str,
    pub server_name: &'a str,
    /// The cluster's k3s version, so the server matches it.
    pub k3s_version: &'a str,
    /// The team's Tailscale auth key, when it has one saved.
    pub tailscale_key: Option<&'a str>,
    /// A build-only server (join.sh --builder).
    pub builder: bool,
    /// Pyo's registry door and the cluster's pull token there, when Pyo is
    /// on a tailnet (its builds are pulled through it).
    pub registry: Option<(&'a str, &'a str)>,
}

/// A k3s version as k3s reports it (`v1.37.2+k3s1`), which join.sh
/// installs on the new server.
pub fn is_k3s_version(v: &str) -> bool {
    let Some((kube, k3s)) = v.strip_prefix('v').and_then(|r| r.split_once("+k3s")) else {
        return false;
    };
    let num = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    num(k3s) && kube.split('.').count() == 3 && kube.split('.').all(num)
}

/// Single-quoted for the shell.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn command(base: &str, j: &Join) -> String {
    let mut c = format!(
        "curl -fsSL {base}/join.sh | bash -s -- --server {} --token {} --name {}",
        q(j.server_ip),
        q(j.token),
        q(j.server_name)
    );
    if !j.k3s_version.is_empty() {
        c.push_str(&format!(" --k3s {}", q(j.k3s_version)));
    }
    if let Some(k) = j.tailscale_key {
        c.push_str(&format!(" --tailscale-key {}", q(k)));
    }
    if j.builder {
        c.push_str(" --builder");
    }
    if let Some((r, t)) = j.registry {
        c.push_str(&format!(" --registry {} --registry-token {}", q(r), q(t)));
    }
    c
}

/// Pyo on the machine it runs on: where "This machine"'s installer
/// reaches it. PYO_LOCAL_URL, else the API's host port.
pub fn local_api_url() -> String {
    std::env::var("PYO_LOCAL_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8000".into())
}

/// The command that makes a machine a new cluster connected to Pyo.
/// `registry`: Pyo's registry door on the tailnet and the cluster's pull
/// token there.
pub fn connect_command(
    base: &str,
    api: &str,
    token: &str,
    tailscale_key: Option<&str>,
    registry: Option<(&str, &str)>,
) -> String {
    let mut c = format!(
        "curl -fsSL {base}/connect.sh | bash -s -- --api {} --token {}",
        q(api),
        q(token)
    );
    if let Some(k) = tailscale_key {
        c.push_str(&format!(" --tailscale-key {}", q(k)));
    }
    if let Some((r, t)) = registry {
        c.push_str(&format!(" --registry {} --registry-token {}", q(r), q(t)));
    }
    c
}

/// The command that brings a connected cluster's agent install up to date
/// with Pyo's: connect.sh run on the cluster's first server, which reads
/// Pyo's address and the token from the agent already there.
pub fn refresh_command(base: &str) -> String {
    format!("curl -fsSL {base}/connect.sh | bash")
}

/// GET /install/join.sh
pub async fn serve() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/x-shellscript; charset=utf-8",
        )],
        SCRIPT,
    )
}

/// GET /install/connect.sh
pub async fn serve_connect() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/x-shellscript; charset=utf-8",
        )],
        CONNECT_SCRIPT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// connect.sh, join.sh and install.sh carry the AppArmor workaround's
    /// profile: it must be the asset's, character for character.
    #[test]
    fn scripts_carry_the_apparmor_profile() {
        let profile = include_str!("../assets/cri-containerd.apparmor.d").trim_end();
        let install = include_str!("../../../deploy/install.sh");
        for (name, script) in [
            ("connect.sh", CONNECT_SCRIPT),
            ("join.sh", SCRIPT),
            ("deploy/install.sh", install),
        ] {
            let body = script
                .split("<<'PYO_APPARMOR_PROFILE'\n")
                .nth(1)
                .and_then(|r| r.split("\nPYO_APPARMOR_PROFILE\n").next())
                .unwrap_or_else(|| panic!("{name} has no AppArmor profile"));
            assert_eq!(
                body, profile,
                "{name}'s AppArmor profile differs from assets/cri-containerd.apparmor.d"
            );
        }
    }

    /// small_machine's boot-time helper follows the machine's memory: k3s's
    /// GOMEMLIMIT under 3.5 GB (pyo-proto's small tier), nothing from there.
    #[test]
    fn memory_helper_follows_memory() {
        let helper = CONNECT_SCRIPT
            .split("<<'MEMORY'\n")
            .nth(1)
            .and_then(|r| r.split("\nMEMORY\n").next())
            .expect("connect.sh has the memory helper");
        let dir = std::env::temp_dir().join(format!("pyo-memory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let k3s_env = |mib: u64| {
            let meminfo = dir.join(format!("meminfo-{mib}"));
            std::fs::write(&meminfo, format!("MemTotal: {} kB\n", mib * 1024)).unwrap();
            let out = std::process::Command::new("sh")
                .args(["-c", helper, "pyo-memory.sh", "k3s-env"])
                .env("PYO_MEMINFO", &meminfo)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        // 40% of the memory, at least 768 MiB.
        assert_eq!(k3s_env(1945), "GOMEMLIMIT=778MiB");
        assert_eq!(k3s_env(1024), "GOMEMLIMIT=768MiB");
        assert_eq!(
            k3s_env(pyo_proto::memory::SMALL_MIB - 1),
            format!(
                "GOMEMLIMIT={}MiB",
                (pyo_proto::memory::SMALL_MIB - 1) * 2 / 5
            )
        );
        assert_eq!(k3s_env(pyo_proto::memory::SMALL_MIB), "");
        assert_eq!(k3s_env(16384), "");
    }

    /// The helper's k3s config drop-in: what kubelet sets aside for k3s and
    /// the system, by memory, so the scheduler doesn't count it as room.
    #[test]
    fn kubelet_sets_aside_memory_for_k3s() {
        let helper = CONNECT_SCRIPT
            .split("<<'MEMORY'\n")
            .nth(1)
            .and_then(|r| r.split("\nMEMORY\n").next())
            .expect("connect.sh has the memory helper");
        let dir = std::env::temp_dir().join(format!("pyo-reserved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = |meminfo: &str| {
            let info = dir.join("meminfo");
            std::fs::write(&info, meminfo).unwrap();
            let out = dir.join("config.yaml.d/50-pyo-memory.yaml");
            let status = std::process::Command::new("sh")
                .args(["-c", helper, "pyo-memory.sh", "k3s-config"])
                .arg(&out)
                .env("PYO_MEMINFO", &info)
                .status()
                .unwrap();
            assert!(status.success());
            std::fs::read_to_string(&out).ok()
        };
        let mib = |m: u64| format!("MemTotal: {} kB\n", m * 1024);
        for (mem, kube, system) in [
            (960, 192, 64),
            (1973, 384, 128),
            (pyo_proto::memory::SMALL_MIB - 1, 384, 128),
            (pyo_proto::memory::SMALL_MIB, 512, 256),
            (pyo_proto::memory::LARGE_MIB, 768, 256),
        ] {
            let text = config(&mib(mem)).expect("a drop-in");
            assert!(text.contains("\nkubelet-arg+:\n"), "{text}");
            assert!(
                text.contains(&format!("  - \"kube-reserved=memory={kube}Mi\"\n  - \"system-reserved=memory={system}Mi\"\n")),
                "{mem}: {text}"
            );
        }
        // Memory it can't read: no drop-in (the one before is removed).
        assert_eq!(config("nothing\n"), None);
        // The unit runs it at every start, at the path small_machine writes.
        let block = script_block("small-machine", CONNECT_SCRIPT);
        assert!(
            block.contains("K3S_MEMORY_CONFIG=/etc/rancher/k3s/config.yaml.d/50-pyo-memory.yaml\n")
        );
        assert!(block.contains(
            "\"ExecStartPre=-/usr/local/lib/pyo-memory.sh k3s-config $K3S_MEMORY_CONFIG\""
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scripts that install k3s, Tailscale and NVIDIA's toolkit share
    /// their installer code (`# >>> pyo:<name>` … `# <<< pyo:<name>`), and
    /// those that carry k3s's pin carry deploy/k3s.env's.
    #[test]
    fn scripts_share_the_pinned_installers() {
        let block = |name: &str, file: &str, script: &str| -> String {
            let start = script
                .find(&format!("# >>> pyo:{name} "))
                .unwrap_or_else(|| panic!("{file} has no pyo:{name} block"));
            let end = script[start..]
                .find(&format!("# <<< pyo:{name}\n"))
                .unwrap_or_else(|| panic!("{file}'s pyo:{name} block has no end"));
            script[start..start + end].to_owned()
        };
        let install = include_str!("../../../deploy/install.sh");
        let setup_host = include_str!("../../../scripts/setup-host.sh");
        let join_node = include_str!("../../../scripts/join-node.sh");
        let k3s = block("k3s", "connect.sh", CONNECT_SCRIPT);
        for (file, script) in [
            ("join.sh", SCRIPT),
            ("deploy/install.sh", install),
            ("scripts/setup-host.sh", setup_host),
            ("scripts/join-node.sh", join_node),
        ] {
            assert_eq!(
                block("k3s", file, script),
                k3s,
                "{file}'s k3s installer differs from connect.sh's"
            );
        }
        let repos = block("repos", "connect.sh", CONNECT_SCRIPT);
        let pins: String = include_str!("../../../deploy/k3s.env")
            .lines()
            .filter(|l| l.starts_with("PYO_K3S_"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(pins.lines().count(), 4);
        for (file, script) in [
            ("connect.sh", CONNECT_SCRIPT),
            ("join.sh", SCRIPT),
            ("scripts/join-node.sh", join_node),
        ] {
            assert_eq!(
                block("repos", file, script),
                repos,
                "{file}'s repository setup differs from connect.sh's"
            );
            assert_eq!(
                block("k3s-pin", file, script),
                format!("# >>> pyo:k3s-pin (deploy/k3s.env)\n{pins}"),
                "{file}'s k3s pin differs from deploy/k3s.env"
            );
        }
        let small = block("small-machine", "connect.sh", CONNECT_SCRIPT);
        for (file, script) in [
            ("join.sh", SCRIPT),
            ("deploy/install.sh", install),
            ("scripts/setup-host.sh", setup_host),
            ("scripts/join-node.sh", join_node),
        ] {
            assert_eq!(
                block("small-machine", file, script),
                small,
                "{file}'s small_machine differs from connect.sh's"
            );
        }
        // How both become root.
        assert_eq!(
            block("become-root", "join.sh", SCRIPT),
            block("become-root", "connect.sh", CONNECT_SCRIPT),
            "join.sh's become_root differs from connect.sh's"
        );
        // How both find Pyo's (or the control plane's) tailnet name.
        assert_eq!(
            block("tailnet-host", "join.sh", SCRIPT),
            block("tailnet-host", "connect.sh", CONNECT_SCRIPT),
            "join.sh's tailnet_host differs from connect.sh's"
        );
        for (file, script) in [
            ("connect.sh", CONNECT_SCRIPT),
            ("join.sh", SCRIPT),
            ("deploy/install.sh", install),
            ("scripts/setup-host.sh", setup_host),
            ("scripts/join-node.sh", join_node),
        ] {
            for piped in ["get.k3s.io", "tailscale.com/install.sh"] {
                assert!(!script.contains(piped), "{file} runs {piped}");
            }
        }
    }

    /// install.sh and scripts/pyo-up.sh set up the machine Pyo runs on the
    /// same way: its timers, bootstrap key and tailnet name.
    #[test]
    fn install_and_pyo_up_share_the_host_setup() {
        let function = |file: &str, script: &str, name: &str| -> String {
            let start = script
                .find(&format!("\n{name}() {{"))
                .unwrap_or_else(|| panic!("{file} has no {name}()"));
            let end = script[start..]
                .find("\n}\n")
                .unwrap_or_else(|| panic!("{file}'s {name}() has no end"));
            script[start..start + end].to_owned()
        };
        let install = include_str!("../../../deploy/install.sh");
        let pyo_up = include_str!("../../../scripts/pyo-up.sh");
        for name in [
            "host_units",
            "bootstrap_key",
            "tailnet_name",
            "remote_access_note",
        ] {
            assert_eq!(
                function("deploy/install.sh", install, name),
                function("scripts/pyo-up.sh", pyo_up, name),
                "deploy/install.sh's {name}() differs from scripts/pyo-up.sh's"
            );
        }
    }

    #[test]
    fn k3s_versions() {
        assert!(is_k3s_version("v1.37.2+k3s1"));
        for v in [
            "",
            "1.37.2+k3s1",
            "v1.37+k3s1",
            "v1.37.2",
            "v1.37.2-eks-1",
            "v1.37.2+k3s",
            "v1.37.2+k3s1;x",
        ] {
            assert!(!is_k3s_version(v), "{v}");
        }
    }

    #[test]
    fn builds_command() {
        let c = command(
            "https://pyo.dev",
            &Join {
                server_ip: "100.64.0.1",
                token: "abc123.0123456789abcdef",
                server_name: "sg-1",
                k3s_version: "v1.36.5+k3s1",
                tailscale_key: None,
                builder: false,
                registry: None,
            },
        );
        assert_eq!(
            c,
            "curl -fsSL https://pyo.dev/join.sh | bash -s -- --server '100.64.0.1' --token 'abc123.0123456789abcdef' --name 'sg-1' --k3s 'v1.36.5+k3s1'"
        );
        let c = command(
            "https://x",
            &Join {
                server_ip: "1",
                token: "t",
                server_name: "n",
                k3s_version: "",
                tailscale_key: Some("tskey-auth-it's"),
                builder: true,
                registry: Some(("100.64.0.1:5005", "tok")),
            },
        );
        assert!(c.ends_with("--tailscale-key 'tskey-auth-it'\\''s' --builder --registry '100.64.0.1:5005' --registry-token 'tok'"));
        assert!(
            [SCRIPT, CONNECT_SCRIPT]
                .iter()
                .all(|s| s.starts_with("#!/usr/bin/env bash"))
        );
        assert_eq!(
            connect_command(
                "https://pyo.dev",
                "http://100.64.0.1:8000",
                "abc",
                None,
                Some(("100.64.0.1:5005", "def"))
            ),
            "curl -fsSL https://pyo.dev/connect.sh | bash -s -- --api 'http://100.64.0.1:8000' --token 'abc' --registry '100.64.0.1:5005' --registry-token 'def'"
        );
        assert_eq!(
            refresh_command("https://pyo.dev"),
            "curl -fsSL https://pyo.dev/connect.sh | bash"
        );
    }

    #[test]
    fn commands_name_pyo_by_its_tailnet_name() {
        let (ip, name) = (Some("100.64.0.1"), Some("server-1.tail1234.ts.net"));
        for (addr, want) in [
            (
                "http://100.64.0.1:8000",
                "http://server-1.tail1234.ts.net:8000",
            ),
            (
                "http://100.64.0.1:8000/install",
                "http://server-1.tail1234.ts.net:8000/install",
            ),
            ("100.64.0.1:5005", "server-1.tail1234.ts.net:5005"),
            ("100.64.0.1", "server-1.tail1234.ts.net"),
            // Another machine's address, or a longer one that starts the same: as it is.
            ("100.64.0.10", "100.64.0.10"),
            ("http://100.64.0.10:8000", "http://100.64.0.10:8000"),
            ("100.64.0.7:5005", "100.64.0.7:5005"),
            ("https://pyo.example.com", "https://pyo.example.com"),
        ] {
            assert_eq!(with_tailnet_name(addr, ip, name), want, "{addr}");
        }
        // No tailnet name (no MagicDNS and HTTPS certificates), or no tailnet IP: the IP.
        assert_eq!(
            with_tailnet_name("http://100.64.0.1:8000", ip, None),
            "http://100.64.0.1:8000"
        );
        assert_eq!(
            with_tailnet_name("100.64.0.1:5005", None, name),
            "100.64.0.1:5005"
        );
        let named = |a: &str| with_tailnet_name(a, ip, name);
        let c = command(
            &named("http://100.64.0.1:8000/install"),
            &Join {
                server_ip: &named("100.64.0.1"),
                token: "t",
                server_name: "server-2",
                k3s_version: "",
                tailscale_key: None,
                builder: false,
                registry: Some((&named("100.64.0.1:5005"), "tok")),
            },
        );
        assert_eq!(
            c,
            "curl -fsSL http://server-1.tail1234.ts.net:8000/install/join.sh | bash -s -- --server 'server-1.tail1234.ts.net' --token 't' --name 'server-2' --registry 'server-1.tail1234.ts.net:5005' --registry-token 'tok'"
        );
        let api = named("http://100.64.0.1:8000");
        assert!(secure_transport(&api));
        assert_eq!(
            pick_scripts_url(None, &[Some(&api)]),
            "http://server-1.tail1234.ts.net:8000/install"
        );
        assert_eq!(
            connect_command(
                &pick_scripts_url(None, &[Some(&api)]),
                &api,
                "abc",
                None,
                Some((&named("100.64.0.1:5005"), "def"))
            ),
            "curl -fsSL http://server-1.tail1234.ts.net:8000/install/connect.sh | bash -s -- --api 'http://server-1.tail1234.ts.net:8000' --token 'abc' --registry 'server-1.tail1234.ts.net:5005' --registry-token 'def'"
        );
    }

    #[test]
    fn scripts_come_from_the_setting_then_pyo_then_pyo_dev() {
        let tailnet = Some("http://100.64.0.1:8000/");
        // The setting wins.
        assert_eq!(
            pick_scripts_url(Some(" https://mirror.example.com/ "), &[tailnet]),
            "https://mirror.example.com"
        );
        // Unset or blank: Pyo, at the first address another machine reaches.
        for unset in [None, Some(""), Some("  ")] {
            assert_eq!(
                pick_scripts_url(
                    unset,
                    &[tailnet, Some("https://server-1.tail1234.ts.net:8443")]
                ),
                "http://100.64.0.1:8000/install"
            );
        }
        assert_eq!(
            pick_scripts_url(
                None,
                &[
                    Some("http://pyo.pyo-system.svc:8000"),
                    None,
                    Some("https://server-1.tail1234.ts.net:8443")
                ]
            ),
            "https://server-1.tail1234.ts.net:8443/install"
        );
        assert_eq!(
            pick_scripts_url(None, &[Some("https://pyo.example.com")]),
            "https://pyo.example.com/install"
        );
        // Only on this machine, or in its cluster: the published scripts.
        let local = [
            "http://localhost:8000",
            "http://127.0.0.1:8000",
            "http://[::1]:8000",
            "http://pyo.pyo-system.svc:8000",
            "http://pyo.pyo-system.svc.cluster.local:8000",
            "pyo.example.com",
            "",
        ];
        for u in local {
            assert_eq!(
                pick_scripts_url(None, &[Some(u)]),
                PUBLISHED_SCRIPTS_URL,
                "{u}"
            );
        }
        assert_eq!(pick_scripts_url(None, &[None, None]), PUBLISHED_SCRIPTS_URL);
    }

    #[test]
    fn plain_http_only_to_private_addresses() {
        for ok in [
            "https://pyo.example.com",
            "https://203.0.113.5:8443/",
            "http://127.0.0.1:8000",
            "http://localhost:8000",
            "http://[::1]:8000",
            "http://10.0.0.5:8000",
            "http://172.16.3.4",
            "http://192.168.1.10:8000",
            "http://100.64.0.1:8000",
            "http://100.127.255.254",
            "http://[fd7a:115c:a1e0::1]:8000",
            "http://pyo.pyo-system.svc:8000",
            "http://pyo.pyo-system.svc.cluster.local:8000",
            "http://server-1.tail1234.ts.net:8000",
        ] {
            assert!(secure_transport(ok), "{ok}");
        }
        for bad in [
            "http://pyo.example.com",
            "http://203.0.113.5:8000",
            "http://100.128.0.1",
            "http://8.8.8.8",
            "http://[2001:db8::1]:8000",
            "http://10.0.0.5@evil.example.com/",
            "http://evil.example.com#@10.0.0.5",
            "http://svc.example.com",
            "ftp://10.0.0.5",
            "pyo.example.com",
            "https://",
            "",
        ] {
            assert!(!secure_transport(bad), "{bad}");
        }
        // Never a plain-http public address for the scripts either.
        assert_eq!(
            pick_scripts_url(None, &[Some("http://pyo.example.com")]),
            PUBLISHED_SCRIPTS_URL
        );
    }

    #[test]
    fn this_machine_gets_its_scripts_from_pyo_on_it() {
        // PYO_LOCAL_URL unset in tests: the API's host port, never pyo.dev.
        if std::env::var("PYO_LOCAL_URL").is_err() {
            assert_eq!(local_scripts_url(), "http://127.0.0.1:8000/install");
            assert_eq!(
                connect_command(&local_scripts_url(), &local_api_url(), "abc", None, None),
                "curl -fsSL http://127.0.0.1:8000/install/connect.sh | bash -s -- --api 'http://127.0.0.1:8000' --token 'abc'"
            );
        }
    }

    /// The part of a script between `# >>> pyo:<name> ` and `# <<< pyo:<name>`.
    fn script_block(name: &str, script: &str) -> String {
        let start = script
            .find(&format!("# >>> pyo:{name} "))
            .unwrap_or_else(|| panic!("no pyo:{name} block"));
        let end = script[start..]
            .find(&format!("# <<< pyo:{name}\n"))
            .unwrap_or_else(|| panic!("pyo:{name} has no end"));
        script[start..start + end].to_owned()
    }

    /// Runs `script` with bash: (exit code, stdout, stderr).
    fn bash(script: &str) -> (i32, String, String) {
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .output()
            .expect("bash runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// `kubectl version -o json` with these client and server minors.
    fn version_json(client: &str, server: &str) -> String {
        format!(
            "{{\n  \"clientVersion\": {{\n    \"major\": \"1\",\n    \"minor\": \"{client}\",\n    \"gitVersion\": \"v1.{client}.2+k3s1\"\n  }},\n  \"kustomizeVersion\": \"v5.7.1\",\n  \"serverVersion\": {{\n    \"major\": \"1\",\n    \"minor\": \"{server}\",\n    \"gitVersion\": \"v1.{server}.0\"\n  }}\n}}\n"
        )
    }

    /// connect.sh's apply_agent with a stub k3s: `version` is what
    /// `kubectl version -o json` prints (empty: it fails), and `apply`
    /// fails the first `fails` times. Sleeping only moves the clock.
    fn apply_agent(version: &str, fails: u32) -> (i32, String, String) {
        let block = script_block("agent-apply", CONNECT_SCRIPT);
        bash(&format!(
            r#"set -euo pipefail
MANIFEST=/dev/null FAILS={fails} applies=0
VERSION=$(cat <<'JSON'
{version}
JSON
)
k3s() {{
  case "$2" in
    version) [ -n "$VERSION" ] && printf '%s' "$VERSION" ;;
    get) return 0 ;;
    apply) applies=$((applies + 1)); [ "$applies" -gt "$FAILS" ] || {{ echo 'context deadline exceeded' >&2; return 1; }} ;;
  esac
}}
sleep() {{ SECONDS=$((SECONDS + $1)); }}
{block}
if apply_agent; then echo "ok after $applies"; else echo "failed after $applies"; fi
"#
        ))
    }

    #[test]
    fn the_server_version_is_read_from_its_own_field() {
        let block = script_block("agent-apply", CONNECT_SCRIPT);
        for (client, server, want) in [("37", "37", "37"), ("37", "29", "29"), ("29", "31+", "31")]
        {
            let json = version_json(client, server);
            let (code, out, _) = bash(&format!(
                "set -euo pipefail\nk3s() {{ cat <<'JSON'\n{json}JSON\n}}\n{block}\nserver_minor\n"
            ));
            assert_eq!((code, out.trim()), (0, want), "{client}/{server}");
        }
        // No answer: nothing, and no failure under set -e.
        let (code, out, _) = bash(&format!(
            "set -euo pipefail\nk3s() {{ return 1; }}\n{block}\nm=$(server_minor)\necho \"[$m]\"\n"
        ));
        assert_eq!((code, out.trim()), (0, "[]"));
    }

    #[test]
    fn the_agent_is_applied_again_until_kubernetes_takes_it() {
        let (code, out, err) = apply_agent(&version_json("37", "37"), 3);
        assert_eq!(code, 0, "{err}");
        assert!(out.trim().ends_with("ok after 4"), "{out}");
        // Progress as it goes: what it's doing, then each try and the time.
        assert!(out.starts_with("Installing the Pyo agent and its admission policy…"));
        assert!(
            err.contains("(try 1, 0s so far). Trying again in 5s…"),
            "{err}"
        );
        assert!(
            err.contains("(try 3, 15s so far). Trying again in 20s…"),
            "{err}"
        );
    }

    /// Each try gives the API server 2 minutes for the policy (not its 34 s
    /// default for a write) and applies server-side.
    #[test]
    fn the_agent_is_applied_with_time_to_spare() {
        let block = script_block("agent-apply", CONNECT_SCRIPT);
        let apply = block.lines().find(|l| l.contains("kubectl apply")).unwrap();
        for flag in [
            "--server-side",
            "--force-conflicts",
            "--field-manager=pyo",
            "--request-timeout=120s",
            "-f \"$MANIFEST\"",
        ] {
            assert!(apply.contains(flag), "{flag}: {apply}");
        }
        assert!(block.contains("until=$((SECONDS + 300))"));
    }

    #[test]
    fn without_its_admission_policy_the_agent_isnt_installed() {
        // 1.30 or later, or a version that can't be read: about 5 minutes
        // of tries, then a failure, never "older than 1.30".
        for version in [version_json("37", "37"), String::new()] {
            let (code, out, err) = apply_agent(&version, u32::MAX);
            assert_eq!(code, 0, "{err}");
            let last = out.trim().lines().last().unwrap();
            assert!(last.starts_with("failed after "), "{out}");
            let tries: u32 = last["failed after ".len()..].parse().unwrap();
            assert!((10..=14).contains(&tries), "{tries} tries");
            assert!(!out.contains("older than") && !err.contains("older than"));
        }
        // Only a server that says it's older goes on without the policy.
        let (code, out, _) = apply_agent(&version_json("37", "29"), u32::MAX);
        assert_eq!(code, 0);
        assert!(out.contains("1.29, older than 1.30"), "{out}");
        assert!(out.trim().ends_with("ok after 1"), "{out}");
    }

    /// Run again on a k3s that's installed (the connect command, install.sh,
    /// setup-host.sh), small_machine brings the memory reserve up to date,
    /// and k3s restarts only when what it runs with changed.
    /// connect.sh's small-machine block with its files under `dir`.
    fn sandboxed_small_machine(dir: &std::path::Path) -> String {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir.join("run")).unwrap();
        let d = dir.display();
        let block = script_block("small-machine", CONNECT_SCRIPT)
            .replace("/usr/local/lib/", &format!("{d}/"))
            .replace("/etc/systemd/system", &format!("{d}/systemd"))
            .replace("/run/", &format!("{d}/run/"))
            .replace(
                "/etc/rancher/k3s/config.yaml.d/50-pyo-memory.yaml",
                &format!("{d}/config.yaml.d/50-pyo-memory.yaml"),
            );
        assert!(!block.contains("/etc/") && !block.contains(" /run/"));
        block
    }

    #[test]
    fn existing_k3s_restarts_only_for_a_new_memory_reserve() {
        let dir = std::env::temp_dir().join(format!("pyo-refresh-{}", std::process::id()));
        let block = sandboxed_small_machine(&dir);
        let d = dir.display();
        let run = |mem: u64, steps: &str| {
            let (code, out, err) = bash(&format!(
                "set -euo pipefail\nsystemctl() {{ [ \"$1\" != restart ] || echo RESTART; }}\n{block}\n\
                 export PYO_MEMINFO={d}/meminfo; echo \"MemTotal: {} kB\" > $PYO_MEMINFO\n\
                 started() {{ {d}/pyo-memory.sh k3s-env > {d}/run/pyo-k3s.env; }}\n{steps}",
                mem * 1024
            ));
            assert_eq!(code, 0, "{err}");
            out.matches("RESTART").count()
        };
        // A k3s from before the reserve: it restarts once to use it.
        assert_eq!(run(1973, "k3s_memory_refresh"), 1);
        assert_eq!(run(1973, "started; k3s_memory_refresh"), 0);
        // The machine got more memory: a new reserve, and no GOMEMLIMIT.
        assert_eq!(run(16384, "k3s_memory_refresh"), 1);
        assert_eq!(run(16384, "started; k3s_memory_refresh"), 0);
        // "later": the caller restarts it.
        assert_eq!(
            run(
                960,
                "k3s_memory_refresh later; [ -n \"$K3S_MEMORY_CHANGED\" ]"
            ),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
        // Each script runs it on a k3s that's already installed.
        for (file, script, call) in [
            ("connect.sh", CONNECT_SCRIPT, "  k3s_memory_refresh later\n"),
            (
                "deploy/install.sh",
                include_str!("../../../deploy/install.sh"),
                "    say \"Using the k3s that's installed\"\n    k3s_memory_refresh\n",
            ),
            (
                "scripts/setup-host.sh",
                include_str!("../../../scripts/setup-host.sh"),
                "  k3s_memory_refresh later\n  systemctl restart k3s\n",
            ),
        ] {
            assert!(
                script.contains(call),
                "{file} doesn't refresh k3s's memory reserve"
            );
        }
        assert!(CONNECT_SCRIPT.contains(
            "if [ -n \"$REGISTRY\" ] || [ -n \"$GPU_TOOLKIT_NEW\" ] || [ -n \"$K3S_MEMORY_CHANGED\" ]; then systemctl restart k3s"
        ));
    }

    /// "Turned on compressed swap" only when it turns on here, not when
    /// it was on already (the connect command run again).
    #[test]
    fn zram_is_reported_only_when_it_turns_on() {
        let dir = std::env::temp_dir().join(format!("pyo-zram-{}", std::process::id()));
        let block = sandboxed_small_machine(&dir);
        let d = dir.display();
        let run = |steps: &str| {
            let (code, out, err) = bash(&format!(
                "set -euo pipefail\n\
                 systemctl() {{ [ \"$*\" != 'enable --now pyo-zram.service' ] || echo /dev/zram0 > {d}/run/pyo-zram; }}\n\
                 {block}\nexport PYO_MEMINFO={d}/meminfo; echo 'MemTotal: 1973000 kB' > $PYO_MEMINFO\n{steps}"
            ));
            assert_eq!(code, 0, "{err}");
            out
        };
        let said = "Turned on compressed swap in memory (zram).";
        assert!(run("small_machine").contains(said));
        // On already: the unit was running, and enabling it changes nothing.
        assert!(!run("small_machine").contains(said));
        let again = run(&format!(
            "rm {d}/run/pyo-zram; small_machine; small_machine"
        ));
        assert_eq!(again.matches(said).count(), 1, "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Servers joined to a cluster (join.sh, scripts/join-node.sh) get the
    /// memory reserve too, on k3s-agent, before k3s starts.
    #[test]
    fn joined_servers_get_the_memory_reserve() {
        let dir = std::env::temp_dir().join(format!("pyo-agent-{}", std::process::id()));
        let block = sandboxed_small_machine(&dir);
        let d = dir.display();
        let (code, out, err) = bash(&format!(
            "set -euo pipefail\nsystemctl() {{ [ \"$1\" != restart ] || echo \"RESTART $2\"; }}\n\
             K3S_UNIT=k3s-agent\n{block}\nexport PYO_MEMINFO={d}/meminfo; echo 'MemTotal: 1973000 kB' > $PYO_MEMINFO\n\
             k3s_memory_refresh"
        ));
        assert_eq!(code, 0, "{err}");
        assert_eq!(out.matches("RESTART k3s-agent").count(), 1, "{out}");
        let unit = std::fs::read_to_string(dir.join("systemd/k3s-agent.service.d/pyo-memory.conf"))
            .expect("k3s-agent's drop-in");
        assert!(unit.contains("EnvironmentFile=-"), "{unit}");
        assert!(
            std::fs::read_to_string(dir.join("systemd/pyo-zram.service"))
                .unwrap()
                .contains("\nBefore=k3s-agent.service\n")
        );
        assert!(!dir.join("systemd/k3s.service.d").exists());
        let _ = std::fs::remove_dir_all(&dir);
        for (file, script, install) in [
            (
                "join.sh",
                SCRIPT,
                "small_machine\nK3S_URL=\"https://$SERVER_IP:6443\" K3S_TOKEN=\"$K3S_TOKEN\" k3s_install \"$K3S_DIR\" agent\n",
            ),
            (
                "scripts/join-node.sh",
                include_str!("../../../scripts/join-node.sh"),
                "small_machine\nif ! command -v k3s >/dev/null; then\n",
            ),
        ] {
            assert!(
                script.contains("\nK3S_UNIT=k3s-agent\n# >>> pyo:small-machine "),
                "{file} doesn't run it on k3s-agent"
            );
            assert!(script.contains(install), "{file} doesn't set it up");
        }
    }

    /// Not root: connect.sh and join.sh run again through sudo, with their
    /// arguments and Tailscale key, from their file or, piped into bash, from
    /// the rest of the pipe. As root, they go straight on. Commands people
    /// copy pipe into plain bash.
    #[test]
    fn scripts_ask_sudo_for_root() {
        let dir = std::env::temp_dir().join(format!("pyo-sudo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let d = dir.display();
        // id says root only under the stub sudo, which notes how it was run.
        let write = |name: &str, body: &str| {
            let path = dir.join("bin").join(name);
            std::fs::write(&path, body).unwrap();
            std::process::Command::new("chmod")
                .arg("755")
                .arg(&path)
                .status()
                .unwrap();
        };
        write(
            "id",
            "#!/bin/sh\nif [ -n \"$STUB_ROOT\" ]; then echo 0; else echo 1000; fi\n",
        );
        write(
            "sudo",
            &format!(
                "#!/bin/sh\necho \"sudo $*\" >> {d}/sudo.log\nexport STUB_ROOT=1\nexec \"$@\"\n"
            ),
        );
        // Nothing else on the PATH but what the scripts' start runs, so
        // "no sudo" is no sudo.
        for tool in ["bash", "cat", "env"] {
            let found = std::process::Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .output()
                .unwrap();
            let found = String::from_utf8(found.stdout).unwrap();
            std::os::unix::fs::symlink(found.trim(), dir.join("bin").join(tool)).unwrap();
        }
        // The script's own start, then what shows it ran (as root or not).
        let rest = "echo \"ran as $(id -u): $* [${TS_AUTHKEY:-}] opts=$-\"\n";
        for (file, script, call) in [
            ("connect.sh", CONNECT_SCRIPT, "set -euo pipefail\n"),
            ("join.sh", SCRIPT, "set -euo pipefail\n"),
        ] {
            let start = &script[..script.find(call).expect(file) + call.len()];
            let path = dir.join(file);
            std::fs::write(&path, format!("{start}{rest}")).unwrap();
            let run = |how: &str, root: bool| {
                let _ = std::fs::remove_file(dir.join("sudo.log"));
                let out = std::process::Command::new("bash")
                    .arg("-c")
                    .arg(how)
                    .env("PATH", format!("{d}/bin"))
                    .env("TS_AUTHKEY", "tskey-1")
                    .env("STUB_ROOT", if root { "1" } else { "" })
                    .env_remove("KUBECONFIG")
                    .current_dir(&dir)
                    .output()
                    .unwrap();
                let sudo = std::fs::read_to_string(dir.join("sudo.log")).unwrap_or_default();
                (
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                    sudo,
                )
            };
            let p = path.display();
            // Piped, as people run it.
            let (out, err, sudo) = run(
                &format!("cat {p} | bash -s -- --api 'http://a b' --token t"),
                false,
            );
            assert_eq!(
                out.trim(),
                "ran as 0: --api http://a b --token t [tskey-1] opts=ehuBs",
                "{file}: {err}"
            );
            assert!(err.contains("Pyo needs root. Asking sudo..."), "{err}");
            assert_eq!(sudo.lines().count(), 1, "{file}: {sudo}");
            assert!(
                sudo.contains("env TS_AUTHKEY=tskey-1 bash -s -- --api"),
                "{sudo}"
            );
            // From a file.
            let (out, _, sudo) = run(&format!("bash {p} --name x"), false);
            assert_eq!(
                out.trim(),
                "ran as 0: --name x [tskey-1] opts=ehuB",
                "{file}"
            );
            assert!(sudo.contains(&format!("bash {p} --name x")), "{sudo}");
            // As root: no sudo.
            let (out, _, sudo) = run(&format!("cat {p} | bash -s -- --name x"), true);
            assert_eq!(
                out.trim(),
                "ran as 0: --name x [tskey-1] opts=ehuBs",
                "{file}"
            );
            assert_eq!(sudo, "", "{file}");
            // No sudo to ask: it says so.
            std::fs::rename(dir.join("bin/sudo"), dir.join("bin/nosudo")).unwrap();
            let (out, err, _) = run(&format!("cat {p} | bash"), false);
            assert_eq!(out, "", "{file}");
            assert!(err.contains("there's no sudo here"), "{err}");
            std::fs::rename(dir.join("bin/nosudo"), dir.join("bin/sudo")).unwrap();
        }
        // connect.sh --uninstall with KUBECONFIG: kubectl as you, no sudo.
        let p = dir.join("connect.sh");
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("cat {} | bash -s -- --uninstall", p.display()))
            .env("PATH", format!("{d}/bin"))
            .env("KUBECONFIG", "/dev/null")
            .env("STUB_ROOT", "")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ran as 1000: --uninstall [] opts=ehuBs"
        );
        let _ = std::fs::remove_dir_all(&dir);
        // The commands people copy.
        for c in [
            refresh_command("https://pyo.example/install"),
            connect_command(
                "https://pyo.example/install",
                "https://pyo.example",
                "t",
                None,
                None,
            ),
        ] {
            assert!(c.contains(" | bash") && !c.contains("sudo"), "{c}");
        }
    }

    #[test]
    fn k3s_gets_room_above_its_live_heap() {
        let block = script_block("small-machine", CONNECT_SCRIPT);
        let helper = block
            .split("<<'MEMORY'\n")
            .nth(1)
            .and_then(|r| r.split("\nMEMORY\n").next())
            .expect("the small-machine block has the memory helper");
        let dir = std::env::temp_dir().join(format!("pyo-heap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (mem, want) in [(960, 768), (1973, 789), (2048, 819), (3583, 1433)] {
            let meminfo = dir.join(format!("meminfo-{mem}"));
            std::fs::write(&meminfo, format!("MemTotal: {} kB\n", mem * 1024)).unwrap();
            let out = std::process::Command::new("sh")
                .args(["-c", &format!("{helper}\ngomemlimit"), "pyo-memory.sh"])
                .env("PYO_MEMINFO", &meminfo)
                .output()
                .unwrap();
            let out = String::from_utf8(out.stdout).unwrap();
            assert_eq!(out.trim(), want.to_string(), "{mem}");
        }
    }
}
