//! The one-line commands Pyo shows for setting up servers. They run
//! generic scripts (assets/, served by every API under /install/ and
//! published at pyo.dev) with the details as arguments, so the scripts hold
//! no secrets:
//! - connect.sh makes a machine a new cluster (k3s and the agent);
//! - join.sh adds a server to an existing cluster (or a build-only server);
//! - sandbox.sh makes a machine a sandbox server (sandbox/join.rs).

/// The scripts, served at /install/join.sh and /install/connect.sh.
pub const SCRIPT: &str = include_str!("../assets/join.sh");
pub const CONNECT_SCRIPT: &str = include_str!("../assets/connect.sh");
/// sandbox.sh makes a machine a sandbox server (sandbox/join.rs). It lives
/// with install.sh, since it sets up a machine outside any cluster.
pub const SANDBOX_SCRIPT: &str = include_str!("../../../deploy/sandbox.sh");

/// Where the scripts are published, for when this Pyo has no address
/// another machine can reach. pyo.dev serves each release's scripts at
/// `/v/<version>/`, only as its signed release.json lists them.
pub const PUBLISHED_SCRIPTS_URL: &str = "https://pyo.dev";

/// This release's scripts on pyo.dev: `https://pyo.dev/v/<version>`.
pub fn published_scripts_url() -> String {
    format!("{PUBLISHED_SCRIPTS_URL}/v/{}", env!("CARGO_PKG_VERSION"))
}

/// Where the commands for other machines fetch their scripts from, first
/// of:
/// 1. `PYO_SCRIPTS_URL`, when set (a mirror, or pyo.dev);
/// 2. Pyo itself (`<url>/install`) at `reach`: the address the command gives
///    the machine for Pyo (the tailnet API, or the public URL people gave),
///    which it has to reach anyway;
/// 3. Pyo on the tailnet ([`crate::web::tailnet_api_url`]), then its HTTPS
///    dashboard;
/// 4. pyo.dev ([`published_scripts_url`]), when Pyo is only on its own
///    machine or in its cluster.
///
/// Only for machines already on the tailnet, or told Pyo's public URL. A
/// command with a Tailscale key uses [`scripts_url_before_tailnet`].
pub fn scripts_url(reach: Option<&str>) -> String {
    let configured = std::env::var("PYO_SCRIPTS_URL").ok();
    let tailnet = crate::web::tailnet_api_url().map(|u| friendly(&u));
    pick_scripts_url(
        configured.as_deref(),
        &[
            reach,
            tailnet.as_deref(),
            crate::web::public_url().as_deref(),
        ],
    )
}

fn pick_scripts_url(configured: Option<&str>, pyo: &[Option<&str>]) -> String {
    if let Some(u) = setting(configured) {
        return u;
    }
    match pyo
        .iter()
        .flatten()
        .map(|u| u.trim().trim_end_matches('/'))
        .find(|u| outside(u) && secure_transport(u))
    {
        Some(u) => format!("{u}/install"),
        None => published_scripts_url(),
    }
}

/// Where a command that gives the machine a Tailscale key (`--tailscale-key`)
/// fetches its script. The machine joins the tailnet only once the script
/// runs, so the script can't come from Pyo on the tailnet. Nor from Pyo on
/// its public IP: that address may not be reachable from outside (a server
/// behind NAT, like Starlink's). So:
/// 1. `PYO_SCRIPTS_URL`, when set;
/// 2. else this release's scripts on pyo.dev ([`published_scripts_url`]),
///    and join.sh's `--k3s` keeps the server on the cluster's k3s.
///
/// A dev build's version may not be a published release, so pyo.dev has no
/// scripts for it: dev builds set `PYO_SCRIPTS_URL` (to a mirror, or a Pyo
/// the new machine can reach).
///
/// Commands for machines already on the tailnet use [`scripts_url`].
pub fn scripts_url_before_tailnet() -> String {
    pick_scripts_url_before_tailnet(std::env::var("PYO_SCRIPTS_URL").ok().as_deref())
}

pub(crate) fn pick_scripts_url_before_tailnet(configured: Option<&str>) -> String {
    setting(configured).unwrap_or_else(published_scripts_url)
}

/// `PYO_SCRIPTS_URL`, when it's set to something. pyo.dev itself means
/// this release's scripts there.
fn setting(configured: Option<&str>) -> Option<String> {
    configured
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(|u| u.trim_end_matches('/'))
        .map(|u| {
            if u == PUBLISHED_SCRIPTS_URL {
                published_scripts_url()
            } else {
                u.to_owned()
            }
        })
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
        crate::web::tailnet_ip().as_deref(),
        crate::web::tailnet_name().as_deref(),
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

/// join.sh's arguments: each flag, and its value when it takes one.
pub fn args(j: &Join) -> Vec<(&'static str, Option<String>)> {
    let mut a = vec![
        ("--server", Some(j.server_ip.to_owned())),
        ("--token", Some(j.token.to_owned())),
        ("--name", Some(j.server_name.to_owned())),
    ];
    if !j.k3s_version.is_empty() {
        a.push(("--k3s", Some(j.k3s_version.to_owned())));
    }
    if let Some(k) = j.tailscale_key {
        a.push(("--tailscale-key", Some(k.to_owned())));
    }
    if j.builder {
        a.push(("--builder", None));
    }
    if let Some((r, t)) = j.registry {
        a.push(("--registry", Some(r.to_owned())));
        a.push(("--registry-token", Some(t.to_owned())));
    }
    a
}

/// Arguments on a command line, values single-quoted.
fn quoted(args: &[(&str, Option<String>)]) -> String {
    args.iter()
        .map(|(f, v)| match v {
            Some(v) => format!("{f} {}", q(v)),
            None => (*f).to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn command(base: &str, j: &Join) -> String {
    format!(
        "curl -fsSL {base}/join.sh | bash -s -- {}",
        quoted(&pinned(base, args(j)))
    )
}

/// Puts a cluster's server on the cluster's tailnet and moves its k3s there
/// (connect.sh --tailnet-only): a cluster connected without Tailscale does
/// this on its control-plane server before more servers join it.
pub fn tailnet_only_command(base: &str, key: &str) -> String {
    format!(
        "curl -fsSL {base}/connect.sh | bash -s -- {}",
        quoted(&pinned(
            base,
            vec![
                ("--tailnet-only", None),
                ("--tailscale-key", Some(key.to_owned())),
            ]
        ))
    )
}

/// A script's arguments for where it's fetched from (`base`). A Pyo's own
/// scripts and pyo.dev's `/v/<version>/` are this release's. From anywhere
/// else (a mirror) they get `--version`: a release's join.sh and connect.sh
/// then run that release's script from pyo.dev when they're another's.
pub fn pinned(
    base: &str,
    mut args: Vec<(&'static str, Option<String>)>,
) -> Vec<(&'static str, Option<String>)> {
    let base = base.trim_end_matches('/');
    if !base.ends_with("/install") && !base.starts_with(&format!("{PUBLISHED_SCRIPTS_URL}/v/")) {
        args.push(("--version", Some(env!("CARGO_PKG_VERSION").to_owned())));
    }
    args
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
    format!(
        "curl -fsSL {base}/connect.sh | bash -s -- {}",
        quoted(&pinned(
            base,
            connect_args(api, token, tailscale_key, registry)
        ))
    )
}

/// connect.sh's arguments, as [`args`].
pub fn connect_args(
    api: &str,
    token: &str,
    tailscale_key: Option<&str>,
    registry: Option<(&str, &str)>,
) -> Vec<(&'static str, Option<String>)> {
    let mut a = vec![
        ("--api", Some(api.to_owned())),
        ("--token", Some(token.to_owned())),
    ];
    if let Some(k) = tailscale_key {
        a.push(("--tailscale-key", Some(k.to_owned())));
    }
    if let Some((r, t)) = registry {
        a.push(("--registry", Some(r.to_owned())));
        a.push(("--registry-token", Some(t.to_owned())));
    } else if crate::deploy::in_cluster_registries() {
        a.push(("--local-registry", None));
    }
    a
}

/// The command that brings a connected cluster's agent install up to date
/// with Pyo's: connect.sh run on the cluster's first server, which reads
/// Pyo's address and the token from the agent already there.
pub fn refresh_command(base: &str) -> String {
    match quoted(&pinned(base, Vec::new())) {
        a if a.is_empty() => format!("curl -fsSL {base}/connect.sh | bash"),
        a => format!("curl -fsSL {base}/connect.sh | bash -s -- {a}"),
    }
}

/// Pyo's scripts are plain text, so a browser shows one instead of
/// downloading it (`curl … | bash` doesn't mind). web::headers makes every
/// response nosniff.
const SCRIPT_TYPE: &str = "text/plain; charset=utf-8";

fn script(body: &'static str) -> impl axum::response::IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, SCRIPT_TYPE)], body)
}

/// GET /install/join.sh
pub async fn serve() -> impl axum::response::IntoResponse {
    script(SCRIPT)
}

/// GET /install/connect.sh
pub async fn serve_connect() -> impl axum::response::IntoResponse {
    script(CONNECT_SCRIPT)
}

/// GET /install/sandbox.sh
pub async fn serve_sandbox() -> impl axum::response::IntoResponse {
    script(SANDBOX_SCRIPT)
}

/// install.sh's `host_units`: the scripts and timers on the machine Pyo
/// runs on. The host's tailnet timer runs it whenever it changes, so an
/// update from the dashboard brings them too.
static HOST_UNITS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let install = include_str!("../../../deploy/install.sh");
    let mut lines = install.lines().skip_while(|l| *l != "host_units() {");
    let mut body = String::new();
    // Its end is the first "}" line outside the scripts it writes (their
    // own functions end the same way).
    let mut heredoc: Option<String> = None;
    for line in lines.by_ref() {
        body.push_str(line);
        body.push('\n');
        match &heredoc {
            Some(end) if line == end => heredoc = None,
            Some(_) => {}
            None if line == "}" => break,
            None => {
                if let Some((_, rest)) = line.split_once("<<'") {
                    heredoc = rest.split_once('\'').map(|(d, _)| d.to_owned());
                }
            }
        }
    }
    assert!(body.ends_with("\n}\n"), "install.sh's host_units() ends");
    format!("#!/usr/bin/env bash\nset -euo pipefail\n{body}host_units\n")
});

/// GET /install/host-units.sh
pub async fn serve_host_units() -> impl axum::response::IntoResponse {
    script(HOST_UNITS.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scripts_are_plain_text() {
        use axum::response::IntoResponse;
        for res in [
            serve().await.into_response(),
            serve_connect().await.into_response(),
            serve_sandbox().await.into_response(),
            serve_host_units().await.into_response(),
        ] {
            assert_eq!(
                res.headers()[axum::http::header::CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
        }
    }

    /// This release's scripts on pyo.dev.
    const V: &str = concat!("https://pyo.dev/v/", env!("CARGO_PKG_VERSION"));
    /// From a mirror, a release's scripts run this release's.
    const PIN: &str = concat!(" --version '", env!("CARGO_PKG_VERSION"), "'");

    #[test]
    fn host_units_script_is_install_sh_s() {
        let s = HOST_UNITS.as_str();
        assert!(s.starts_with("#!/usr/bin/env bash\nset -euo pipefail\nhost_units() {\n"));
        assert!(s.ends_with("\n}\nhost_units\n"));
        assert!(s.contains("/usr/local/bin/pyo-reset-login"));
        assert!(s.contains("tee /usr/local/lib/pyo/addresses.sh"));
        assert!(s.contains("http://127.0.0.1:8000/install/host-units.sh"));
        assert!(s.contains("systemctl -q enable --now pyo-connect.timer pyo-tailnet.timer"));
    }

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

    /// k3s over the tailnet: its own drop-in, written only when it changes
    /// (Pyo's server restarts k3s only then).
    #[test]
    fn k3s_tailnet_drop_in() {
        let start = CONNECT_SCRIPT.find("# >>> pyo:k3s-tailnet ").unwrap();
        let end = CONNECT_SCRIPT.find("# <<< pyo:k3s-tailnet\n").unwrap();
        let block = &CONNECT_SCRIPT[start..end];
        let dir = std::env::temp_dir().join(format!("pyo-k3s-tailnet-{}", std::process::id()));
        let file = dir.join("config.yaml.d/40-pyo-tailnet.yaml");
        let run = |args: &str| {
            std::process::Command::new("bash")
                .args(["-c", &format!("{block}\nk3s_tailnet_config {args}")])
                .env("K3S_TAILNET_CONFIG", &file)
                .status()
                .unwrap()
                .success()
        };
        assert!(run("100.64.0.7 100.64.0.7 pyo-1.tail1234.ts.net"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "node-ip: \"100.64.0.7\"\nflannel-iface: tailscale0\ntls-san+:\n  - \"100.64.0.7\"\n  - \"pyo-1.tail1234.ts.net\"\n"
        );
        assert!(
            !run("100.64.0.7 100.64.0.7 pyo-1.tail1234.ts.net"),
            "the same: nothing to do"
        );
        assert!(run("100.64.0.8"), "a new address");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "node-ip: \"100.64.0.8\"\nflannel-iface: tailscale0\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        // Memory it can't read: no reservations, the disk settings still.
        let text = config("nothing\n").expect("a drop-in");
        assert!(!text.contains("reserved="), "{text}");
        assert!(
            text.contains("\nkubelet-arg+:\n  - \"eviction-hard="),
            "{text}"
        );
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

    /// The same drop-in: kubelet frees disk space only under 5% free (unused
    /// images well before), and the server stops counting as short of disk
    /// once 1 GiB more is free, not k3s's 10% more. Whatever the memory.
    #[test]
    fn kubelet_frees_disk_space_late_and_lets_go_soon() {
        let helper = CONNECT_SCRIPT
            .split("<<'MEMORY'\n")
            .nth(1)
            .and_then(|r| r.split("\nMEMORY\n").next())
            .expect("connect.sh has the memory helper");
        let dir = std::env::temp_dir().join(format!("pyo-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let want = "  - \"eviction-hard=imagefs.available<5%,nodefs.available<5%\"\n\
                    \x20 - \"eviction-minimum-reclaim=imagefs.available=1Gi,nodefs.available=1Gi\"\n\
                    \x20 - \"image-gc-high-threshold=85\"\n\
                    \x20 - \"image-gc-low-threshold=75\"\n";
        for meminfo in [
            "MemTotal: 983040 kB\n",
            "MemTotal: 33554432 kB\n",
            "nothing\n",
        ] {
            let info = dir.join("meminfo");
            std::fs::write(&info, meminfo).unwrap();
            let out = dir.join("50-pyo-memory.yaml");
            let status = std::process::Command::new("sh")
                .args(["-c", helper, "pyo-memory.sh", "k3s-config"])
                .arg(&out)
                .env("PYO_MEMINFO", &info)
                .status()
                .unwrap();
            assert!(status.success());
            let text = std::fs::read_to_string(&out).unwrap();
            assert!(text.ends_with(want), "{meminfo}: {text}");
            // One list, which k3s adds to its own arguments.
            assert_eq!(text.matches("kubelet-arg").count(), 1, "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        // Image cleanup starts and ends before eviction does (95% used).
        let pct = |key: &str| -> u32 {
            want.split(&format!("{key}="))
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap()
                .parse()
                .unwrap()
        };
        assert!(pct("image-gc-low-threshold") < pct("image-gc-high-threshold"));
        assert!(pct("image-gc-high-threshold") < 95);
    }

    /// A server set up before the disk settings gets them, and k3s restarts
    /// once for them, when its script runs again.
    #[test]
    fn existing_k3s_restarts_once_for_the_disk_settings() {
        let dir = std::env::temp_dir().join(format!("pyo-disk-refresh-{}", std::process::id()));
        let block = sandboxed_small_machine(&dir);
        let d = dir.display();
        let (code, out, err) = bash(&format!(
            "set -euo pipefail\nsystemctl() {{ [ \"$1\" != restart ] || echo RESTART; }}\n{block}\n\
             export PYO_MEMINFO={d}/meminfo; echo 'MemTotal: 16777216 kB' > $PYO_MEMINFO\n\
             mkdir -p {d}/config.yaml.d\n\
             printf '%s\\n' '# Managed by Pyo: what kubelet sets aside.' 'kubelet-arg+:' '  - \"kube-reserved=memory=768Mi\"' '  - \"system-reserved=memory=256Mi\"' > {d}/config.yaml.d/50-pyo-memory.yaml\n\
             : > {d}/run/pyo-k3s.env\n\
             k3s_memory_refresh\nk3s_memory_refresh"
        ));
        assert_eq!(code, 0, "{err}");
        assert_eq!(out.matches("RESTART").count(), 1, "{out}");
        let text = std::fs::read_to_string(dir.join("config.yaml.d/50-pyo-memory.yaml")).unwrap();
        assert!(text.contains("eviction-minimum-reclaim="), "{text}");
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
        let progress = block("progress", "connect.sh", CONNECT_SCRIPT);
        for (file, script) in [
            ("join.sh", SCRIPT),
            ("deploy/install.sh", install),
            (
                "scripts/pyo-up.sh",
                include_str!("../../../scripts/pyo-up.sh"),
            ),
            ("deploy/sandbox.sh", SANDBOX_SCRIPT),
        ] {
            assert_eq!(
                block("progress", file, script),
                progress,
                "{file}'s progress helpers differ from connect.sh's"
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
        // How they move k3s onto the tailnet, and how both wait for Tailnet Lock.
        let k3s_tailnet = block("k3s-tailnet", "connect.sh", CONNECT_SCRIPT);
        for (file, script) in [("join.sh", SCRIPT), ("deploy/install.sh", install)] {
            assert_eq!(
                block("k3s-tailnet", file, script),
                k3s_tailnet,
                "{file}'s k3s_tailnet_config differs from connect.sh's"
            );
        }
        assert_eq!(
            block("tailnet-lock", "join.sh", SCRIPT),
            block("tailnet-lock", "connect.sh", CONNECT_SCRIPT),
            "join.sh's tailnet_lock_wait differs from connect.sh's"
        );
        // How connect.sh and the installer find ports 80 and 443 in use.
        assert_eq!(
            block("web-ports", "deploy/install.sh", install),
            block("web-ports", "connect.sh", CONNECT_SCRIPT),
            "deploy/install.sh's web_ports_busy differs from connect.sh's"
        );
        // How both become root.
        assert_eq!(
            block("become-root", "join.sh", SCRIPT),
            block("become-root", "connect.sh", CONNECT_SCRIPT),
            "join.sh's become_root differs from connect.sh's"
        );
        // How they run the release --version names (release.yml writes
        // each one's release on its pyo:release-version line).
        let release = block("release", "connect.sh", CONNECT_SCRIPT);
        for (file, script) in [("join.sh", SCRIPT), ("deploy/sandbox.sh", SANDBOX_SCRIPT)] {
            assert_eq!(
                block("release", file, script),
                release,
                "{file}'s run_release differs from connect.sh's"
            );
        }
        for (file, script) in [
            ("connect.sh", CONNECT_SCRIPT),
            ("join.sh", SCRIPT),
            ("deploy/sandbox.sh", SANDBOX_SCRIPT),
        ] {
            assert_eq!(
                script.matches(RELEASE_LINE).count(),
                1,
                "{file} needs exactly one {RELEASE_LINE:?} line for release.yml"
            );
        }
        // How both find Pyo's (or the control plane's) tailnet name.
        assert_eq!(
            block("tailnet-host", "join.sh", SCRIPT),
            block("tailnet-host", "connect.sh", CONNECT_SCRIPT),
            "join.sh's tailnet_host differs from connect.sh's"
        );
        // sandbox.sh joins the tailnet the same way.
        for name in ["repos", "tailnet-lock", "tailnet-host"] {
            assert_eq!(
                block(name, "deploy/sandbox.sh", SANDBOX_SCRIPT),
                block(name, "connect.sh", CONNECT_SCRIPT),
                "deploy/sandbox.sh's pyo:{name} block differs from connect.sh's"
            );
        }
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
    /// same way: its timers, bootstrap key and where Pyo is (host_units
    /// writes addresses.sh).
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
        for name in ["host_units", "bootstrap_key", "pyo_addresses"] {
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
        assert_eq!(published_scripts_url(), V);
        let c = command(
            V,
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
            format!(
                "curl -fsSL {V}/join.sh | bash -s -- --server '100.64.0.1' --token 'abc123.0123456789abcdef' --name 'sg-1' --k3s 'v1.36.5+k3s1'"
            )
        );
        let c = command(
            "https://x/install",
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
                V,
                "http://100.64.0.1:8000",
                "abc",
                None,
                Some(("100.64.0.1:5005", "def"))
            ),
            format!(
                "curl -fsSL {V}/connect.sh | bash -s -- --api 'http://100.64.0.1:8000' --token 'abc' --registry '100.64.0.1:5005' --registry-token 'def'"
            )
        );
        assert_eq!(
            refresh_command(V),
            format!("curl -fsSL {V}/connect.sh | bash")
        );
        // A mirror's: --version runs this release's, when they're another's.
        assert_eq!(
            refresh_command("https://mirror.example.com"),
            format!("curl -fsSL https://mirror.example.com/connect.sh | bash -s --{PIN}")
        );
        // Pyo's own scripts are its release's.
        assert_eq!(
            refresh_command("https://pyo.example/install"),
            "curl -fsSL https://pyo.example/install/connect.sh | bash"
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
        // The setting wins. pyo.dev there is this release's scripts on it.
        assert_eq!(
            pick_scripts_url(Some(" https://mirror.example.com/ "), &[tailnet]),
            "https://mirror.example.com"
        );
        assert_eq!(pick_scripts_url(Some("https://pyo.dev/"), &[tailnet]), V);
        // Unset or blank: Pyo, at the first address another machine reaches.
        for unset in [None, Some(""), Some("  ")] {
            assert_eq!(
                pick_scripts_url(unset, &[tailnet, Some("https://pyo.tail1234.ts.net")]),
                "http://100.64.0.1:8000/install"
            );
        }
        assert_eq!(
            pick_scripts_url(
                None,
                &[
                    Some("http://pyo.pyo-system.svc:8000"),
                    None,
                    Some("https://pyo.tail1234.ts.net")
                ]
            ),
            "https://pyo.tail1234.ts.net/install"
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
            assert_eq!(pick_scripts_url(None, &[Some(u)]), V, "{u}");
        }
        assert_eq!(pick_scripts_url(None, &[None, None]), V);
    }

    #[test]
    fn a_machine_off_the_tailnet_fetches_scripts_from_the_setting_or_pyo_dev() {
        let pick = pick_scripts_url_before_tailnet;
        assert_eq!(
            pick(Some(" https://mirror.example.com/ ")),
            "https://mirror.example.com"
        );
        // Unset or blank: pyo.dev, never Pyo itself (on the tailnet, or on
        // a public IP that may sit behind NAT).
        for unset in [None, Some(""), Some("  ")] {
            assert_eq!(pick(unset), V);
        }
    }

    #[test]
    fn commands_with_a_tailscale_key_fetch_their_script_off_the_tailnet() {
        let base = pick_scripts_url_before_tailnet(None);
        // join.sh: this release's on pyo.dev, and --k3s keeps the server on
        // the cluster's k3s.
        assert_eq!(
            command(
                &base,
                &Join {
                    server_ip: "server-1.tail1234.ts.net",
                    token: "t",
                    server_name: "server-2",
                    k3s_version: "v1.37.2+k3s1",
                    tailscale_key: Some("tskey-auth-1"),
                    builder: false,
                    registry: None,
                },
            ),
            format!(
                "curl -fsSL {V}/join.sh | bash -s -- --server 'server-1.tail1234.ts.net' --token 't' --name 'server-2' --k3s 'v1.37.2+k3s1' --tailscale-key 'tskey-auth-1'"
            )
        );
        assert_eq!(
            connect_command(
                &base,
                "https://pyo.tail1234.ts.net",
                "abc",
                Some("tskey-auth-1"),
                Some(("pyo-registry.tail1234.ts.net", "def"))
            ),
            format!(
                "curl -fsSL {V}/connect.sh | bash -s -- --api 'https://pyo.tail1234.ts.net' --token 'abc' --tailscale-key 'tskey-auth-1' --registry 'pyo-registry.tail1234.ts.net' --registry-token 'def'"
            )
        );
        // A mirror the operator set.
        assert!(
            connect_command(
                &pick_scripts_url_before_tailnet(Some("https://mirror.example.com")),
                "https://pyo.tail1234.ts.net",
                "abc",
                Some("k"),
                None
            )
            .starts_with("curl -fsSL https://mirror.example.com/connect.sh | ")
        );
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
        assert_eq!(pick_scripts_url(None, &[Some("http://pyo.example.com")]), V);
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

    /// The line release.yml writes a script's release on.
    const RELEASE_LINE: &str = "\n  local release=\"\" # pyo:release-version\n";

    /// join.sh's run_release, as release.yml publishes it for `release`
    /// (empty: as Pyo serves it), with `args`. pyo.dev has only 0.4.2's
    /// join.sh, which prints its arguments and exits with 3.
    fn run_release(release: &str, args: &str) -> (i32, String, String) {
        let block = script_block("release", SCRIPT).replace(
            RELEASE_LINE,
            &format!("\n  local release=\"{release}\" # pyo:release-version\n"),
        );
        bash(&format!(
            r#"curl() {{
  local out="" url="" a
  while [ $# -gt 0 ]; do
    case "$1" in -o) out=$2; shift 2 ;; --proto | --retry) shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac
  done
  [ "$url" = https://pyo.dev/v/0.4.2/join.sh ] || return 22
  printf '%s\n' 'echo "0.4.2 join.sh: $*"; exit 3' >"$out"
}}
{block}
run_release join.sh {args}
echo "this one runs"
"#
        ))
    }

    #[test]
    fn scripts_run_the_release_their_version_names() {
        // Pyo 0.4.2's commands give pyo.dev's latest script --version 0.4.2.
        let (code, out, _) = run_release("0.4.3", "--server s --version 0.4.2");
        assert_eq!(
            (code, out.as_str()),
            (3, "0.4.2 join.sh: --server s --version 0.4.2\n")
        );
        let (code, out, _) = run_release("0.4.3", "--version v0.4.2");
        assert_eq!(
            (code, out.as_str()),
            (3, "0.4.2 join.sh: --version v0.4.2\n")
        );
        // Its own release, no --version, or Pyo's own copy: this script.
        for (release, args) in [
            ("0.4.3", "--server s --version 0.4.3"),
            ("0.4.3", "--server s"),
            ("", "--version 0.4.2"),
        ] {
            assert_eq!(
                run_release(release, args),
                (0, "this one runs\n".into(), String::new()),
                "{release} {args}"
            );
        }
        // A release pyo.dev doesn't have, or not a release at all.
        let (code, out, err) = run_release("0.4.3", "--version 0.3.0");
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(
            err.contains("pyo.dev has no join.sh for Pyo 0.3.0."),
            "{err}"
        );
        let (code, _, err) = run_release("0.4.3", "--version '0.4.2;x'");
        assert_eq!(code, 2);
        assert!(err.contains("isn't one"), "{err}");
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

    /// connect.sh's web_ports_busy with k3s and ss stubbed (`stubs` sets
    /// what they answer): "busy [WEB_PORTS] [WEB_BY] <line>" or "free".
    fn web_ports(stubs: &str) -> String {
        let block = script_block("web-ports", CONNECT_SCRIPT);
        let (code, out, err) = bash(&format!(
            r#"set -euo pipefail
UP=1 SVCS="" PODS="" CHART="" TSVC="" SS=""
{stubs}
k3s() {{
  case "$*" in
    "kubectl get --raw=/readyz"*) [ -n "$UP" ] ;;
    "kubectl get services -A"*) printf '%b' "$SVCS" ;;
    "kubectl get pods -A"*) printf '%b' "$PODS" ;;
    "kubectl -n kube-system get helmcharts.helm.cattle.io traefik") [ -n "$CHART" ] ;;
    "kubectl -n kube-system get service traefik") [ -n "$TSVC" ] ;;
    *) echo "unexpected: k3s $*" >&2; exit 9 ;;
  esac
}}
ss() {{ printf '%b' "$SS"; }}
{block}
if web_ports_busy; then echo "busy [$WEB_PORTS] [$WEB_BY] $(web_ports_line)"; else echo "free"; fi
"#
        ));
        assert_eq!(code, 0, "{err}");
        out.trim().to_owned()
    }

    #[test]
    fn connect_finds_what_already_answers_on_80_and_443() {
        let line = |ports: &str, by: &str, text: &str| {
            format!("busy [{ports}] [{by}] {text} Public traffic stays off, so your sites keep working.")
        };
        // k3s's own Traefik, as a k3s installed without Pyo has it.
        assert_eq!(
            web_ports(
                r#"CHART=1 TSVC=1
SVCS="kube-system traefik 80 443\npyo-gateway envoy-pyo-1a2b 80 443\n"
PODS="kube-system svclb-traefik-9f8e-x2 80 443\nkube-system coredns-7b  \nkube-system svclb-envoy-pyo-1a2b-q 80 443\n""#
            ),
            line("80 443", "Traefik", "Ports 80 and 443 are in use by Traefik.")
        );
        // Its chart is there and its Service not yet: Traefik on its way.
        assert_eq!(
            web_ports("CHART=1"),
            line("80 443", "Traefik", "Ports 80 and 443 are in use by Traefik.")
        );
        // Traefik's chart with its Service gone elsewhere (not a load balancer): free.
        assert_eq!(web_ports("CHART=1 TSVC=1"), "free");
        // ingress-nginx on host ports.
        assert_eq!(
            web_ports(r#"PODS="ingress-nginx ingress-nginx-controller-5c 80 443 8443\n""#),
            line("80 443", "ingress-nginx", "Ports 80 and 443 are in use by ingress-nginx.")
        );
        // Another load balancer on 443 only, by its name.
        assert_eq!(
            web_ports(r#"SVCS="shop web-lb 443 8080\n""#),
            line("443", "web-lb", "Port 443 is in use by web-lb.")
        );
        // Pyo's own gateway, and pods without host ports: free.
        assert_eq!(
            web_ports(
                r#"SVCS="pyo-gateway envoy-pyo-1a2b 80 443\n"
PODS="kube-system svclb-envoy-pyo-1a2b-q 80 443\npyo-system pyo-agent-6d  \n""#
            ),
            "free"
        );
        // nginx on this machine, no k3s yet.
        assert_eq!(
            web_ports(
                r#"UP=""
SS='LISTEN 0      511          0.0.0.0:80        0.0.0.0:*    users:(("nginx",pid=812,fd=6),("nginx",pid=811,fd=6))\nLISTEN 0      511             [::]:80           [::]:*    users:(("nginx",pid=812,fd=7))\n'"#
            ),
            line("80", "nginx", "Port 80 is in use by nginx.")
        );
        // Apache on 443 through an interface, Caddy on 80.
        assert_eq!(
            web_ports(
                r#"SS='LISTEN 0 4096 *:80 *:* users:(("caddy",pid=9,fd=3))\nLISTEN 0 511 10.0.0.5%eth0:443 0.0.0.0:* users:(("apache2",pid=4,fd=4))\n'"#
            ),
            line("80 443", "Caddy", "Ports 80 and 443 are in use by Caddy.")
        );
        // A program whose name ss can't tell.
        assert_eq!(
            web_ports(r#"SS="LISTEN 0 4096 *:443 *:*\n""#),
            line("443", "", "Port 443 is in use.")
        );
        // Only this machine, or only the tailnet, reaches them: free.
        assert_eq!(
            web_ports(
                r#"SS='LISTEN 0 4096 127.0.0.1:80 0.0.0.0:* users:(("x",pid=1,fd=3))\nLISTEN 0 4096 [::1]:443 [::]:* users:(("x",pid=1,fd=4))\nLISTEN 0 4096 100.101.102.103:443 0.0.0.0:* users:(("y",pid=2,fd=3))\nLISTEN 0 4096 [fd7a:115c:a1e0::9]:80 [::]:* users:(("y",pid=2,fd=4))\n'"#
            ),
            "free"
        );
        // Nothing at all.
        assert_eq!(web_ports(""), "free");
        // The manifest is asked for with it, and ufw left alone.
        assert!(CONNECT_SCRIPT.contains(r#""$API/install/agent.yaml?token=$TOKEN$WEB_QUERY""#));
        assert!(CONNECT_SCRIPT.contains("WEB_QUERY=\"&webPorts=busy\""));
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
        let progress = script_block("progress", CONNECT_SCRIPT);
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
{progress}
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
        // The step once, then with the time every 20 seconds off a
        // terminal, then done, and no Kubernetes errors or tries.
        assert!(out.starts_with("→ Installing the Pyo agent…\n"), "{out}");
        assert!(!out.contains("agent… 0:00"), "{out}");
        assert!(out.contains("→ Installing the Pyo agent… 0:20"), "{out}");
        assert!(out.contains("\n✓ Installed the Pyo agent "), "{out}");
        assert!(!out.contains("try ") && !err.contains("try "), "{out}{err}");
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
            assert!(out.contains("\n✗ Installing the Pyo agent\n"), "{out}");
            let tries: u32 = last["failed after ".len()..].parse().unwrap();
            assert!((10..=14).contains(&tries), "{tries} tries");
            assert!(!out.contains("older than") && !err.contains("older than"));
        }
        // Only a server that says it's older goes on without the policy.
        let (code, out, _) = apply_agent(&version_json("37", "29"), u32::MAX);
        assert_eq!(code, 0);
        assert!(
            !out.contains("older than") && !out.contains("Kubernetes"),
            "{out}"
        );
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
            "if [ -n \"$REGISTRY\" ] || [ -n \"$LOCAL_REGISTRY_NEW\" ] || [ -n \"$GPU_TOOLKIT_NEW\" ] || [ -n \"$K3S_MEMORY_CHANGED\" ]; then systemctl restart k3s"
        ));
    }

    /// A small machine is set up without a word: compressed swap turns on,
    /// and running it again changes nothing.
    #[test]
    fn small_machine_setup_is_quiet() {
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
        assert_eq!(run("small_machine"), "");
        assert!(dir.join("run/pyo-zram").exists());
        assert_eq!(run("small_machine; small_machine"), "");
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
                "small_machine\nexport K3S_URL=\"https://$SERVER_IP:6443\" K3S_TOKEN\nquietly \"Installing k3s and joining the cluster…\" k3s_install \"$K3S_DIR\" agent ||\n",
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

    /// A script install.sh's host_units writes to /usr/local/lib/pyo.
    fn host_script(name: &str) -> String {
        let install = include_str!("../../../deploy/install.sh");
        let head = format!("tee /usr/local/lib/pyo/{name} >/dev/null <<'SH'\n");
        let start = install
            .find(&head)
            .unwrap_or_else(|| panic!("host_units writes no {name}"))
            + head.len();
        let end = install[start..].find("\nSH\n").unwrap();
        install[start..start + end + 1].to_owned()
    }

    /// Pyo's own server as connect-this-machine.sh sees it: its files in a
    /// directory of the test's, Pyo's answers and connect.sh stubbed.
    struct OwnServer {
        dir: std::path::PathBuf,
    }

    impl OwnServer {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("pyo-own-server-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("bootstrap-key"), "k3y\n").unwrap();
            Self { dir }
        }

        /// One run in `mode` ("auto", "refresh" or none) with Pyo
        /// answering `answer` and connect.sh ending with `rc`: its exit
        /// code and output.
        fn run(&self, mode: &str, answer: &str, rc: i32) -> (i32, String) {
            let d = self.dir.display().to_string();
            let script = host_script("connect-this-machine.sh")
                .replace("/etc/pyo/", &format!("{d}/"))
                .replace("/run/pyo-connect.lock", &format!("{d}/lock"));
            assert!(!script.contains("/etc/pyo") && !script.contains("/run/"), "{script}");
            let (code, out, err) = bash(&format!(
                r#"T='{d}'
ANSWER='{answer}'
curl() {{
  local a
  case "$*" in
    *"http://127.0.0.1:8000/install/this-machine?key=k3y") printf '%s' "$ANSWER" ;;
    *"http://127.0.0.1:8000/install/this-machine/refresh"*)
      for a in "$@"; do case "$a" in install=* | status=*) printf '%s ' "${{a#*=}}" >> "$T/told" ;; esac; done
      echo >> "$T/told" ;;
    *"http://127.0.0.1:8000/install/connect.sh") echo "echo connect.sh ran; echo x >> '$T/ran'; exit {rc}" ;;
    *) echo "unexpected: curl $*" >&2; return 9 ;;
  esac
}}
k3s() {{ [ ! -e "$T/no-policy" ]; }}
systemctl() {{ echo "$*" >> "$T/systemctl"; }}
flock() {{ echo "$*" >> "$T/flock"; }}
set -- {mode}
{script}"#
            ));
            assert_eq!(err, "", "{out}");
            (code, out)
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.dir.join(file)).unwrap_or_default()
        }

        /// How often connect.sh ran.
        fn ran(&self) -> usize {
            self.read("ran").lines().count()
        }

        /// What the host told Pyo, one "install status" per line.
        fn told(&self) -> Vec<String> {
            self.read("told").lines().map(|l| l.trim().to_owned()).collect()
        }

        /// Makes `file` look written `minutes` ago.
        fn age(&self, file: &str, minutes: u32) {
            let ok = std::process::Command::new("touch")
                .args(["-d", &format!("{minutes} minutes ago")])
                .arg(self.dir.join(file))
                .status()
                .unwrap();
            assert!(ok.success());
        }
    }

    impl Drop for OwnServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const ASKED: &str = r#"{"install":"0.51.0-abc","refresh":true,"state":"connected"}"#;
    const CURRENT: &str = r#"{"install":"0.51.0-abc","state":"connected"}"#;

    /// The timer's run every minute: nothing while Pyo doesn't ask, then
    /// connect.sh once for the install, however long Pyo goes on asking.
    #[test]
    fn pyo_s_own_server_refreshes_once_when_pyo_asks() {
        let s = OwnServer::new("once");
        for answer in [CURRENT, r#"{"state":"connected"}"#, r#"{"state":"waiting"}"#, ""] {
            assert_eq!(s.run("auto", answer, 0), (0, String::new()), "{answer}");
        }
        // Nor does it connect the machine: the connect timer does that.
        let connect = r#"{"command":"echo connecting","state":"connect"}"#;
        assert_eq!(s.run("auto", connect, 0), (0, String::new()));
        assert_eq!((s.ran(), s.told().len()), (0, 0));

        let (code, out) = s.run("auto", ASKED, 0);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "Refreshing this server for Pyo 0.51.0.\nconnect.sh ran\nThis server is up to date.\n"
        );
        assert_eq!(s.ran(), 1);
        assert_eq!(s.told(), ["0.51.0-abc running", "0.51.0-abc done"]);
        assert_eq!(s.read("refresh-tried"), "0.51.0-abc\n");
        assert!(!s.dir.join("refresh-failed").exists());
        // It never waits for the lock, and never stops the connect timer.
        assert_eq!(s.read("flock").lines().last(), Some("-n 9"));
        assert_eq!(s.read("systemctl"), "");

        // Pyo still asks (the agent hasn't said it's current, or it never
        // will): not again, a day later either.
        for _ in 0..5 {
            assert_eq!(s.run("auto", ASKED, 0), (0, String::new()));
        }
        s.age("refresh-tried", 3 * 24 * 60);
        assert_eq!(s.run("auto", ASKED, 0), (0, String::new()));
        assert_eq!(s.ran(), 1);
        assert_eq!(s.told().last().unwrap(), "0.51.0-abc done");

        // Another release: once for it. And not again for the one before,
        // should two Pyos answer in turn while one replaces the other.
        let next = ASKED.replace("0.51.0-abc", "0.52.0-def");
        assert_eq!(s.run("auto", &next, 0).0, 0);
        assert_eq!(s.ran(), 2);
        for answer in [ASKED, next.as_str(), ASKED] {
            assert_eq!(s.run("auto", answer, 0), (0, String::new()));
        }
        assert_eq!(s.ran(), 2);
        assert_eq!(s.read("refresh-tried"), "0.51.0-abc\n0.52.0-def\n");
    }

    /// A refresh that failed comes again after an hour, not every minute,
    /// at most 4 times a day, and at once for another Pyo version.
    #[test]
    fn a_failed_refresh_of_pyo_s_own_server_is_tried_again_hourly() {
        let s = OwnServer::new("failed");
        let (code, out) = s.run("auto", ASKED, 1);
        // The timer's unit doesn't fail with it.
        assert_eq!(code, 0);
        assert!(
            out.ends_with("Couldn't refresh this server. Pyo tries again in an hour.\n"),
            "{out}"
        );
        assert_eq!(s.read("refresh-failed"), "0.51.0-abc\n");
        assert_eq!(s.told(), ["0.51.0-abc running", "0.51.0-abc failed"]);
        // The next minutes: nothing but its word to Pyo.
        for _ in 0..3 {
            assert_eq!(s.run("auto", ASKED, 1), (0, String::new()));
        }
        s.age("refresh-failed", 59);
        assert_eq!(s.run("auto", ASKED, 1), (0, String::new()));
        assert_eq!(s.ran(), 1);
        assert_eq!(s.told().last().unwrap(), "0.51.0-abc failed");
        // An hour later: again, and again failed.
        s.age("refresh-failed", 61);
        assert_eq!(s.run("auto", ASKED, 1).0, 0);
        assert_eq!(s.ran(), 2);
        // Four in a day, then no more however old the failure.
        for _ in 0..4 {
            s.age("refresh-failed", 61);
            s.run("auto", ASKED, 1);
        }
        assert_eq!(s.ran(), 4);
        assert_eq!(s.read("refresh-runs").lines().count(), 4);
        assert_eq!(s.told().last().unwrap(), "0.51.0-abc held");
        // Another Pyo version would run at once, but for the day's four.
        let next = ASKED.replace("0.51.0-abc", "0.51.1-abc");
        assert_eq!(s.run("auto", &next, 0), (0, String::new()));
        assert_eq!(s.told().last().unwrap(), "0.51.1-abc held");
        // A day on (the runs are older): the new version at once, and a
        // success clears the failure.
        std::fs::write(s.dir.join("refresh-runs"), "1000\n1001\n1002\n1003\n").unwrap();
        let (code, out) = s.run("auto", &next, 0);
        assert_eq!(code, 0);
        assert!(out.ends_with("This server is up to date.\n"), "{out}");
        assert_eq!(s.ran(), 5);
        assert!(!s.dir.join("refresh-failed").exists());
        assert_eq!(s.read("refresh-runs").lines().count(), 1);
        // The one that failed, an hour on: once more, and it's done.
        let two = OwnServer::new("failed-then-done");
        two.run("auto", ASKED, 1);
        two.age("refresh-failed", 61);
        two.run("auto", ASKED, 0);
        assert_eq!((two.ran(), two.dir.join("refresh-failed").exists()), (2, false));
        assert_eq!(two.run("auto", ASKED, 0), (0, String::new()));
        assert_eq!(two.ran(), 2);
    }

    /// install.sh after an update: the refresh now, whether or not Pyo
    /// asks yet, and the timer doesn't run it again for that install.
    #[test]
    fn the_installer_s_refresh_counts_for_the_timer() {
        let s = OwnServer::new("installer");
        let (code, out) = s.run("refresh", CURRENT, 0);
        assert_eq!(code, 0);
        assert!(out.starts_with("Refreshing this server for Pyo 0.51.0.\n"), "{out}");
        assert_eq!(s.ran(), 1);
        // It waits for the lock, and leaves the day's automatic runs alone.
        assert_eq!(s.read("flock").lines().last(), Some("9"));
        assert_eq!(s.read("refresh-runs"), "");
        assert_eq!(s.run("auto", ASKED, 0), (0, String::new()));
        assert_eq!(s.ran(), 1);
        // One that fails says so to install.sh, and the timer takes over
        // an hour later.
        let f = OwnServer::new("installer-failed");
        assert_eq!(f.run("refresh", ASKED, 1).0, 1);
        assert_eq!(f.run("auto", ASKED, 0), (0, String::new()));
        f.age("refresh-failed", 61);
        f.run("auto", ASKED, 0);
        assert_eq!(f.ran(), 2);
        // A Pyo that doesn't keep its servers current (no install in its
        // answer): as before, connect.sh only for a missing policy.
        let c = OwnServer::new("installer-cloud");
        assert_eq!(c.run("refresh", r#"{"state":"connected"}"#, 0), (0, String::new()));
        assert_eq!(c.run("", r#"{"state":"connected"}"#, 0), (0, String::new()));
        assert_eq!(c.ran(), 0);
        assert!(c.read("systemctl").contains("disable --now pyo-connect.timer"));
    }

    /// Only a stamp is taken from Pyo's answer, and only with a quoted
    /// "refresh":true.
    #[test]
    fn the_host_reads_only_a_stamp_from_pyo_s_answer() {
        let s = OwnServer::new("stamp");
        for answer in [
            r#"{"install":"","refresh":true,"state":"connected"}"#,
            r#"{"install":"a b; touch x","refresh":true,"state":"connected"}"#,
            r#"{"install":"$(touch x)","refresh":true,"state":"connected"}"#,
            r#"{"install":"0.51.0-abc","refresh":false,"state":"connected"}"#,
        ] {
            assert_eq!(s.run("auto", answer, 0), (0, String::new()), "{answer}");
        }
        assert_eq!(s.ran(), 0);
    }

    /// The tailnet timer's script (every minute, for good) brings this
    /// Pyo's host scripts first, then asks for the refresh with them.
    #[test]
    fn the_minute_timer_updates_its_scripts_then_refreshes() {
        let s = host_script("tailnet-this-machine.sh");
        let units = s.find("exec bash -c \"$units\"").unwrap();
        let auto = s.find("\nexec /usr/local/lib/pyo/connect-this-machine.sh auto\n").unwrap();
        assert!(units < auto);
        assert!(s.ends_with("connect-this-machine.sh auto\n"));
        // install.sh refreshes after an update, and connects a new install.
        let install = include_str!("../../../deploy/install.sh");
        assert!(install.contains("quietly \"Refreshing this server…\" /usr/local/lib/pyo/connect-this-machine.sh refresh ||"));
        assert!(install.contains("quietly \"Connecting this machine to Pyo…\" /usr/local/lib/pyo/connect-this-machine.sh ||"));
        // What they say is plain.
        for script in [host_script("connect-this-machine.sh"), s] {
            for line in script.lines().filter(|l| l.trim_start().starts_with("echo \"")) {
                assert!(!line.contains(';') && !line.contains(" - "), "{line}");
            }
        }
    }

    /// connect.sh's own_server_lock with flock stubbed: "locked" when it
    /// took the lock, "theirs" when it came with it, "none" elsewhere.
    fn own_server_lock(setup: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "pyo-own-lock-{}-{}",
            std::process::id(),
            setup.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        let block = script_block("own-server-lock", CONNECT_SCRIPT)
            .replace("/usr/local/lib/pyo/", &format!("{d}/"))
            .replace("/run/pyo-connect.lock", &format!("{d}/lock"));
        let (code, out, err) = bash(&format!(
            r#"set -euo pipefail
T='{d}'
flock() {{ echo "flock $*"; [ "$*" != "-n 9" ] || [ -z "${{BUSY:-}}" ]; }}
{setup}
{block}
own_server_lock
if [ "$(readlink "/proc/$$/fd/9" 2>/dev/null)" = "$T/lock" ]; then echo has-lock; else echo no-lock; fi
"#
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        out.trim().replace('\n', ", ")
    }

    /// A connect.sh run by hand on Pyo's own server takes the lock its
    /// timers and installer hold, and doesn't wait for itself under them.
    #[test]
    fn connect_takes_turns_with_pyo_s_own_runs_on_its_server() {
        // Another cluster's server: no lock at all.
        assert_eq!(own_server_lock(""), "no-lock");
        let own = r#": > "$T/connect-this-machine.sh""#;
        assert_eq!(own_server_lock(own), "flock -n 9, has-lock");
        // Pyo's timer runs: it waits, and says so.
        assert_eq!(
            own_server_lock(&format!("{own}\nBUSY=1")),
            "flock -n 9, Waiting for another Pyo setup on this server to finish., flock 9, has-lock"
        );
        // Run by the timer or the installer, which hold it: theirs.
        assert_eq!(own_server_lock(&format!("{own}\nexec 9>\"$T/lock\"")), "has-lock");
        // Before --tailnet-only's exit and after --uninstall's.
        let at = CONNECT_SCRIPT.find("\n[ -n \"$TAILNET_ONLY\" ] || own_server_lock\n").unwrap();
        assert!(CONNECT_SCRIPT.find("\n  uninstall\n  exit 0\n").unwrap() < at);
        assert!(at < CONNECT_SCRIPT.find("\nnvidia_gpu\n").unwrap());
    }
}
