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

/// Each release's assets on GitHub, its install scripts among them:
/// `<url>/v<version>/connect.sh`. pyo.dev doesn't serve a release
/// candidate's scripts, so a candidate's commands fetch them here.
pub const RELEASES_URL: &str = "https://github.com/upsetdev/pyo/releases/download";

/// This release's published scripts: `https://pyo.dev/v/<version>`, or
/// for a release candidate its GitHub release.
pub fn published_scripts_url() -> String {
    published_scripts_url_for(env!("CARGO_PKG_VERSION"))
}

/// A release's published scripts ([`published_scripts_url`]).
fn published_scripts_url_for(version: &str) -> String {
    if is_release_candidate(version) {
        format!("{RELEASES_URL}/v{version}")
    } else {
        format!("{PUBLISHED_SCRIPTS_URL}/v/{version}")
    }
}

/// A release candidate's version: `X.Y.Z-rc.N`.
pub const fn is_release_candidate(version: &str) -> bool {
    let b = version.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i] != b'-' {
        i += 1;
    }
    if b.len() < i + 5 || b[i + 1] != b'r' || b[i + 2] != b'c' || b[i + 3] != b'.' {
        return false;
    }
    i += 4;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            return false;
        }
        i += 1;
    }
    true
}

/// Where the commands for other machines fetch their scripts from, first
/// of:
/// 1. `PYO_SCRIPTS_URL`, when set (a mirror, or pyo.dev);
/// 2. Pyo itself (`<url>/install`) at `reach`: the address the command gives
///    the machine for Pyo (the tailnet API, or the public URL people gave),
///    which it has to reach anyway;
/// 3. Pyo on the tailnet ([`crate::web::tailnet_api_url`]), then its HTTPS
///    dashboard;
/// 4. this release's published scripts ([`published_scripts_url`]), when
///    Pyo is only on its own machine or in its cluster.
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
/// 2. else this release's published scripts ([`published_scripts_url`]),
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

/// Moves a single-server cluster's server to its team's tailnet
/// (connect.sh --tailnet-only --tailnet): it leaves the tailnet it's on,
/// joins `suffix`'s with `key`, and its k3s follows.
pub fn tailnet_move_command(base: &str, key: &str, suffix: &str) -> String {
    format!(
        "curl -fsSL {base}/connect.sh | bash -s -- {}",
        quoted(&pinned(
            base,
            vec![
                ("--tailnet-only", None),
                ("--tailnet", Some(suffix.to_owned())),
                ("--tailscale-key", Some(key.to_owned())),
            ]
        ))
    )
}

/// A script's arguments for where it's fetched from (`base`). A Pyo's own
/// scripts and the published ones ([`published_scripts_url`]) are this
/// release's. From anywhere else (a mirror) they get `--version`: a
/// release's join.sh and connect.sh then run that release's published
/// script when they're another's.
pub fn pinned(
    base: &str,
    mut args: Vec<(&'static str, Option<String>)>,
) -> Vec<(&'static str, Option<String>)> {
    let base = base.trim_end_matches('/');
    if !base.ends_with("/install")
        && !base.starts_with(&format!("{PUBLISHED_SCRIPTS_URL}/v/"))
        && base != published_scripts_url()
    {
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

/// connect.sh's flag that sets up the verifier on the cluster's first
/// server (`pyo-verify`, on a timer there): it keeps the cluster on what
/// this Pyo's signed release defines.
pub const AUTO_UPDATE: &str = "--auto-update";
/// connect.sh's flag that leaves the verifier out, and removes one that's
/// there.
pub const NO_AUTO_UPDATE: &str = "--no-auto-update";

/// Which of the two a connect or refresh command carries: none on a Pyo
/// whose agents refresh their install themselves (`self_update`,
/// install.rs), where there's no verifier. On one that hands out no
/// self-update role (Pyo Cloud), [`AUTO_UPDATE`] unless the team unticked
/// it (`keep` off), then [`NO_AUTO_UPDATE`].
///
/// Pyo always renders the ticked one. The choice is never kept: the
/// dashboard's checkbox swaps the flag in the command it shows, so what
/// decides is the command the customer runs as root, never Pyo.
pub fn auto_update_flag(self_update: bool, keep: bool) -> Option<&'static str> {
    match (self_update, keep) {
        (true, _) => None,
        (false, true) => Some(AUTO_UPDATE),
        (false, false) => Some(NO_AUTO_UPDATE),
    }
}

/// connect.sh's flag for Pyo's public address, where a server gets its
/// install files (the agent's manifest, the gateway's CRDs) and its
/// verifier looks for releases. connect.sh keeps it for later runs.
pub const INSTALL_URL: &str = "--install-url";

/// What [`INSTALL_URL`] names on a Pyo whose agents reach it at another
/// address (`PYO_AGENT_ENDPOINT`, [`crate::agent_tls::endpoint`]): Pyo
/// Cloud's agent address takes only the agents' own requests, never
/// `/install/`. Pyo's public API address (`PYO_PUBLIC_API_URL`), else its
/// dashboard's. None on any other Pyo, where the agent's address serves
/// both (a refresh reads it from the agent's Secret).
pub fn install_url() -> Option<String> {
    let endpoint = crate::agent_tls::endpoint()?;
    pick_install_url(
        &endpoint,
        std::env::var("PYO_PUBLIC_API_URL").ok().as_deref(),
        crate::web::public_url().as_deref(),
    )
}

fn pick_install_url(endpoint: &str, configured: Option<&str>, dashboard: Option<&str>) -> Option<String> {
    let url = [configured, dashboard]
        .into_iter()
        .flatten()
        .map(|u| u.trim().trim_end_matches('/'))
        .find(|u| !u.is_empty())?;
    (url != endpoint.trim_end_matches('/') && outside(url) && secure_transport(url)).then(|| url.to_owned())
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
    connect_command_with(
        base,
        api,
        token,
        tailscale_key,
        registry,
        auto_update_flag(crate::install::self_update(), true),
        // Not "This machine"'s, from Pyo on it.
        install_url().is_some() && outside(api),
    )
}

/// [`connect_command`] with this [`auto_update_flag`]. With `install`,
/// also [`INSTALL_URL`]: `api`, the public address it connects at, so
/// connect.sh keeps it for refreshes run without it.
fn connect_command_with(
    base: &str,
    api: &str,
    token: &str,
    tailscale_key: Option<&str>,
    registry: Option<(&str, &str)>,
    auto_update: Option<&'static str>,
    install: bool,
) -> String {
    let mut args = connect_args(api, token, tailscale_key, registry);
    if install {
        args.push((INSTALL_URL, Some(api.to_owned())));
    }
    args.extend(auto_update.map(|flag| (flag, None)));
    format!(
        "curl -fsSL {base}/connect.sh | bash -s -- {}",
        quoted(&pinned(base, args))
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
/// Pyo's address and the token from the agent already there. Where the
/// agent's address isn't for installs, with [`INSTALL_URL`].
pub fn refresh_command(base: &str) -> String {
    refresh_command_with(
        base,
        auto_update_flag(crate::install::self_update(), true),
        install_url().as_deref(),
    )
}

/// [`refresh_command`] with this [`auto_update_flag`] and [`install_url`].
fn refresh_command_with(base: &str, auto_update: Option<&'static str>, install: Option<&str>) -> String {
    let mut args: Vec<(&'static str, Option<String>)> = install
        .map(|u| (INSTALL_URL, Some(u.to_owned())))
        .into_iter()
        .collect();
    args.extend(auto_update.map(|flag| (flag, None)));
    match quoted(&pinned(base, args)) {
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

    /// This release's published scripts: on pyo.dev, or a release
    /// candidate's on GitHub.
    const V: &str = if is_release_candidate(env!("CARGO_PKG_VERSION")) {
        concat!(
            "https://github.com/upsetdev/pyo/releases/download/v",
            env!("CARGO_PKG_VERSION")
        )
    } else {
        concat!("https://pyo.dev/v/", env!("CARGO_PKG_VERSION"))
    };
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

    /// pyo.dev serves a release's scripts, but not a release candidate's:
    /// a candidate's commands fetch them from its GitHub release, which
    /// has them as assets (release.yml).
    #[test]
    fn a_release_candidates_scripts_come_from_its_github_release() {
        assert_eq!(
            published_scripts_url_for("0.51.0"),
            "https://pyo.dev/v/0.51.0"
        );
        assert_eq!(
            published_scripts_url_for("0.51.0-rc.1"),
            "https://github.com/upsetdev/pyo/releases/download/v0.51.0-rc.1"
        );
        for v in ["0.51.0-rc.1", "0.51.0-rc.12", "1.0.0-rc.0"] {
            assert!(is_release_candidate(v), "{v}");
        }
        for v in [
            "0.51.0",
            "",
            "0.51.0-rc",
            "0.51.0-rc.",
            "0.51.0-rc.1x",
            "0.51.0-beta.1",
            "0.51.0-",
        ] {
            assert!(!is_release_candidate(v), "{v}");
        }
        // This release's own published scripts need no --version, wherever
        // they are.
        let args = || vec![("--auto-update", None)];
        assert_eq!(pinned(&published_scripts_url(), args()), args());
        assert_eq!(
            pinned(&format!("{}/", published_scripts_url()), args()),
            args()
        );
        assert_eq!(
            refresh_command_with(&published_scripts_url(), None, None),
            format!("curl -fsSL {}/connect.sh | bash", published_scripts_url())
        );
        // Another candidate's assets are a mirror's: pinned to this release.
        let other = "https://github.com/upsetdev/pyo/releases/download/v0.0.1-rc.1";
        assert_eq!(
            refresh_command_with(other, None, None),
            format!("curl -fsSL {other}/connect.sh | bash -s --{PIN}")
        );
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

    /// The verifier's flag: on a Pyo that hands out no self-update role
    /// (Pyo Cloud) the commands carry `--auto-update`, or
    /// `--no-auto-update` when the team unticked it. On any other, neither.
    #[test]
    fn commands_carry_the_auto_update_flag_only_without_self_update() {
        assert_eq!(auto_update_flag(true, true), None);
        assert_eq!(auto_update_flag(true, false), None);
        assert_eq!(auto_update_flag(false, true), Some("--auto-update"));
        assert_eq!(auto_update_flag(false, false), Some("--no-auto-update"));
        let connect = |flag| {
            connect_command_with(
                "https://cloud.example/install",
                "https://cloud.example",
                "abc",
                None,
                None,
                flag,
                false,
            )
        };
        let plain = "curl -fsSL https://cloud.example/install/connect.sh | bash -s -- --api 'https://cloud.example' --token 'abc'";
        // Self-hosted, with self-update: as before.
        assert_eq!(connect(auto_update_flag(true, true)), plain);
        assert_eq!(connect(auto_update_flag(true, false)), plain);
        // Hosted: ticked, then unticked.
        assert_eq!(connect(auto_update_flag(false, true)), format!("{plain} --auto-update"));
        assert_eq!(connect(auto_update_flag(false, false)), format!("{plain} --no-auto-update"));
        let refresh = |base, flag| refresh_command_with(base, flag, None);
        let own = "https://cloud.example/install";
        assert_eq!(
            refresh(own, auto_update_flag(true, true)),
            "curl -fsSL https://cloud.example/install/connect.sh | bash"
        );
        assert_eq!(
            refresh(own, auto_update_flag(false, true)),
            "curl -fsSL https://cloud.example/install/connect.sh | bash -s -- --auto-update"
        );
        assert_eq!(
            refresh(own, auto_update_flag(false, false)),
            "curl -fsSL https://cloud.example/install/connect.sh | bash -s -- --no-auto-update"
        );
        // Before a mirror's --version, as the other flags.
        assert_eq!(
            refresh("https://mirror.example.com", auto_update_flag(false, true)),
            format!("curl -fsSL https://mirror.example.com/connect.sh | bash -s -- --auto-update{PIN}")
        );
        // What Pyo hands out follows this installation, always ticked.
        let handed = auto_update_flag(crate::install::self_update(), true);
        assert_eq!(refresh_command(own), refresh(own, handed));
        assert_eq!(
            connect_command(own, "https://cloud.example", "abc", None, None),
            connect(handed)
        );
        assert_eq!(handed.is_some(), !crate::install::self_update());
        // The servers Pyo makes at a cloud provider take connect.sh's
        // arguments as they are: no verifier is set up without the
        // customer's own command.
        assert!(
            !connect_args("https://cloud.example", "abc", None, None)
                .iter()
                .any(|(flag, _)| flag.contains("auto-update"))
        );
    }

    /// A Pyo whose agents reach it at an address only they may use (Pyo
    /// Cloud's agent address) names its public one for installs: the
    /// refresh, which otherwise takes the agent's, and the connect, whose
    /// --api it is, so the server keeps it. Any other Pyo: as before.
    #[test]
    fn hosted_commands_name_the_install_address() {
        let agent = "https://agent.cloud.example";
        let public = Some("https://cloud.example");
        // Pyo Cloud: PYO_PUBLIC_API_URL, else the dashboard's address.
        assert_eq!(
            pick_install_url(agent, Some(" https://cloud.example/ "), Some("https://pyo.tail1.ts.net")).as_deref(),
            public
        );
        assert_eq!(pick_install_url(agent, None, public).as_deref(), public);
        assert_eq!(pick_install_url(agent, Some(""), public).as_deref(), public);
        // The same address for both, none, or one another machine can't
        // use: nothing to name.
        assert_eq!(pick_install_url(agent, Some("https://agent.cloud.example/"), None), None);
        assert_eq!(pick_install_url(agent, None, None), None);
        assert_eq!(pick_install_url(agent, Some("http://127.0.0.1:8000"), None), None);
        assert_eq!(pick_install_url(agent, Some("http://pyo.example.com"), None), None);
        // Agents with tokens (self-hosted): the address in their Secret serves both.
        assert_eq!(crate::agent_tls::endpoint(), None);
        assert_eq!(install_url(), None);

        let base = "https://cloud.example/install";
        assert_eq!(
            refresh_command_with(base, Some(AUTO_UPDATE), public),
            "curl -fsSL https://cloud.example/install/connect.sh | bash -s -- --install-url 'https://cloud.example' --auto-update"
        );
        assert_eq!(
            refresh_command_with(base, Some(NO_AUTO_UPDATE), public),
            "curl -fsSL https://cloud.example/install/connect.sh | bash -s -- --install-url 'https://cloud.example' --no-auto-update"
        );
        // Before a mirror's --version.
        assert_eq!(
            refresh_command_with("https://mirror.example.com", Some(AUTO_UPDATE), public),
            format!("curl -fsSL https://mirror.example.com/connect.sh | bash -s -- --install-url 'https://cloud.example' --auto-update{PIN}")
        );
        assert_eq!(
            connect_command_with(base, "https://cloud.example", "abc", None, None, Some(AUTO_UPDATE), true),
            "curl -fsSL https://cloud.example/install/connect.sh | bash -s -- --api 'https://cloud.example' --token 'abc' --install-url 'https://cloud.example' --auto-update"
        );

        // Self-hosted: the commands as before, with no install address.
        let own = "https://pyo.example/install";
        assert_eq!(refresh_command(own), "curl -fsSL https://pyo.example/install/connect.sh | bash");
        assert_eq!(
            connect_command(own, "https://pyo.example", "abc", None, None),
            "curl -fsSL https://pyo.example/install/connect.sh | bash -s -- --api 'https://pyo.example' --token 'abc'"
        );
        // "This machine": from Pyo on it.
        assert_eq!(
            connect_command(&local_scripts_url(), &local_api_url(), "abc", None, None),
            "curl -fsSL http://127.0.0.1:8000/install/connect.sh | bash -s -- --api 'http://127.0.0.1:8000' --token 'abc'"
        );
        // The dashboard swaps --auto-update (AgentInstall.tsx): the install
        // address never looks like it.
        assert!(!INSTALL_URL.contains("auto-update"));
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

    /// connect.sh's foreign_operators with k3s stubbed (`stubs` sets what
    /// it answers): its lines, or "none".
    fn foreign(stubs: &str) -> String {
        let block = script_block("foreign-operators", CONNECT_SCRIPT);
        let (code, out, err) = bash(&format!(
            r#"set -euo pipefail
CM_CHART="" EG_CHART="" CM_DEPLOYS="" EG_DEPLOYS="" CM_CRD="" EG_CRD=""
{stubs}
k3s() {{
  case "$*" in
    "kubectl -n kube-system get helmcharts.helm.cattle.io cert-manager -o jsonpath="*) [ -n "$CM_CHART" ] && printf '%s' "$CM_CHART" ;;
    "kubectl -n kube-system get helmcharts.helm.cattle.io envoy-gateway -o jsonpath="*) [ -n "$EG_CHART" ] && printf '%s' "$EG_CHART" ;;
    "kubectl get deployments -A -l app.kubernetes.io/name=cert-manager "*) printf '%b' "$CM_DEPLOYS" ;;
    "kubectl get deployments -A -l control-plane=envoy-gateway "*) printf '%b' "$EG_DEPLOYS" ;;
    "kubectl get crd certificates.cert-manager.io") [ -n "$CM_CRD" ] ;;
    "kubectl get crd envoyproxies.gateway.envoyproxy.io -o jsonpath="*) [ -n "$EG_CRD" ] && printf '%s' "$EG_CRD" ;;
    *) echo "unexpected: k3s $*" >&2; exit 9 ;;
  esac
}}
{block}
if foreign_operators; then :; else echo none; fi
"#
        ));
        assert_eq!(code, 0, "{err}");
        out.trim().to_owned()
    }

    const CM_LINE: &str = "This server already runs cert-manager that Pyo didn't install. Pyo won't change it. Remove it, or connect a server without it.";
    const EG_LINE: &str = "This server already runs Envoy Gateway that Pyo didn't install. Pyo won't change it. Remove it, or connect a server without it.";

    #[test]
    fn connect_passes_a_clean_server_and_pyos_own() {
        // Nothing there yet.
        assert_eq!(foreign(""), "none");
        // Pyo's own, as every version installs it: its charts labelled
        // managed-by=pyo, cert-manager in its namespace, Envoy Gateway in
        // pyo-gateway, Envoy Gateway's CRDs labelled too.
        let own = r#"CM_CHART="cert-manager|pyo" EG_CHART="envoy-gateway|pyo"
CM_DEPLOYS="cert-manager\n" EG_DEPLOYS="pyo-gateway\n" CM_CRD=1
EG_CRD="envoyproxies.gateway.envoyproxy.io|pyo""#;
        assert_eq!(foreign(own), "none");
        // Pyo's charts while they install: nothing running yet.
        assert_eq!(
            foreign(r#"CM_CHART="cert-manager|pyo" EG_CHART="envoy-gateway|pyo" CM_CRD=1"#),
            "none"
        );
        // A first connect that stopped after the gateway's CRDs.
        assert_eq!(foreign(r#"EG_CRD="envoyproxies.gateway.envoyproxy.io|pyo""#), "none");
    }

    #[test]
    fn connect_stops_for_operators_pyo_didnt_install() {
        // cert-manager from Helm or a manifest: running, with its CRDs.
        assert_eq!(foreign(r#"CM_DEPLOYS="cert-manager\n" CM_CRD=1"#), CM_LINE);
        // k3s's own chart of that name, not Pyo's.
        assert_eq!(foreign(r#"CM_CHART="cert-manager|""#), CM_LINE);
        // Only its CRDs.
        assert_eq!(foreign("CM_CRD=1"), CM_LINE);
        // A second one beside Pyo's.
        assert_eq!(
            foreign(r#"CM_CHART="cert-manager|pyo" CM_DEPLOYS="cert-manager\nsecurity\n" CM_CRD=1"#),
            CM_LINE
        );
        // Envoy Gateway from its Helm chart: elsewhere, CRDs without Pyo's label.
        assert_eq!(
            foreign(r#"EG_DEPLOYS="envoy-gateway-system\n" EG_CRD="envoyproxies.gateway.envoyproxy.io|Helm""#),
            EG_LINE
        );
        assert_eq!(foreign(r#"EG_CRD="envoyproxies.gateway.envoyproxy.io|""#), EG_LINE);
        assert_eq!(foreign(r#"EG_CHART="envoy-gateway|""#), EG_LINE);
        // Both: a line each.
        assert_eq!(
            foreign(r#"CM_CRD=1 EG_DEPLOYS="envoy-gateway-system\n""#),
            format!("{CM_LINE}\n{EG_LINE}")
        );
    }

    /// The check runs before the manifest is fetched (which uses the
    /// token) and before anything is applied, and again once k3s is up.
    #[test]
    fn connect_checks_for_foreign_operators_first() {
        let at = |s: &str| CONNECT_SCRIPT.find(s).unwrap_or_else(|| panic!("no {s}"));
        let first = at("\n  no_foreign_operators\n");
        assert!(first < at("/install/agent.yaml?token="));
        let again = at("\nno_foreign_operators\n");
        assert!(at("wait_until -f \"Starting k3s…\"") < again);
        assert!(again < at("\ngateway_crds\n"));
        assert!(again < at("\nif ! apply_agent; then"));
        // Stopped with the line, before anything else.
        let block = script_block("foreign-operators", CONNECT_SCRIPT);
        let (code, _, err) = bash(&format!(
            "set -euo pipefail\nk3s() {{ case \"$*\" in \"kubectl get crd certificates.cert-manager.io\") return 0 ;; *) return 1 ;; esac; }}\n{block}\nno_foreign_operators\necho went on"
        ));
        assert_eq!((code, err.trim()), (1, CM_LINE));
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
                r#"SS='LISTEN 0 4096 127.0.0.1:80 0.0.0.0:* users:(("x",pid=1,fd=3))\nLISTEN 0 4096 [::1]:443 [::]:* users:(("x",pid=1,fd=4))\nLISTEN 0 4096 100.64.0.103:443 0.0.0.0:* users:(("y",pid=2,fd=3))\nLISTEN 0 4096 [fd7a:115c:a1e0::9]:80 [::]:* users:(("y",pid=2,fd=4))\n'"#
            ),
            "free"
        );
        // Nothing at all.
        assert_eq!(web_ports(""), "free");
        // The manifest is asked for with it, and ufw left alone.
        assert!(CONNECT_SCRIPT.contains(r#""$INSTALL_URL/install/agent.yaml?token=$TOKEN$WEB_QUERY""#));
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
            "if [ -n \"$REGISTRY_CHANGED\" ] || [ -n \"$LOCAL_REGISTRY_NEW\" ] || [ -n \"$GPU_TOOLKIT_NEW\" ] || [ -n \"$K3S_MEMORY_CHANGED\" ]; then systemctl restart k3s"
        ));
    }

    /// The connect command with --registry runs on every refresh. k3s reads
    /// the registry's login only when it starts, and a restart takes the
    /// Kubernetes API away for a while, so it restarts for a new address or
    /// login and not when the file already says the same.
    #[test]
    fn existing_k3s_restarts_only_for_a_new_registry_login() {
        let dir = std::env::temp_dir().join(format!("pyo-registry-login-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("registries.yaml");
        let block = script_block("registry-login", CONNECT_SCRIPT).replace(
            "/etc/rancher/k3s/registries.yaml",
            &file.display().to_string(),
        );
        assert!(!block.contains("/etc/"));
        // One run of the script: "changed" or "same".
        let run = |registry: &str, token: &str| {
            let (code, out, err) = bash(&format!(
                "set -euo pipefail\nREGISTRY='{registry}' REGISTRY_TOKEN='{token}'\n{block}\nregistry_login\n\
                 [ -n \"$REGISTRY_CHANGED\" ] && echo changed || echo same"
            ));
            assert_eq!(code, 0, "{err}");
            out.trim().to_owned()
        };
        assert_eq!(run("100.64.0.1:5006", "ab12"), "changed");
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("- \"http://100.64.0.1:5006\"\n"), "{written}");
        assert!(written.contains("password: \"ab12\"\n"), "{written}");
        use std::os::unix::fs::PermissionsExt;
        let mode = |f: &std::path::Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        // The same command again: nothing to restart for, and the file is as it was.
        assert_eq!(run("100.64.0.1:5006", "ab12"), "same");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), written);
        // A new login or a new address is a change.
        assert_eq!(run("100.64.0.1:5006", "cd34"), "changed");
        assert_eq!(run("100.64.0.1:5006", "cd34"), "same");
        assert_eq!(run("100.64.0.2:5006", "cd34"), "changed");
        // A file someone left readable is closed again, without a restart.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(run("100.64.0.2:5006", "cd34"), "same");
        assert_eq!(mode(&file), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
        // The script writes the login through it, and only with --registry.
        assert!(CONNECT_SCRIPT.contains("\n[ -z \"$REGISTRY\" ] || registry_login\n"));
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

    /// connect.sh's connect_lock with flock stubbed, as root unless `setup`
    /// says otherwise: the flock calls it made, then whether it has the lock.
    fn connect_lock(setup: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "pyo-connect-lock-{}-{}",
            std::process::id(),
            setup.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        let block = script_block("connect-lock", CONNECT_SCRIPT)
            .replace("/run/pyo-connect.lock", &format!("{d}/lock"));
        let (code, out, err) = bash(&format!(
            r#"set -euo pipefail
T='{d}'
id() {{ echo "${{ID:-0}}"; }}
flock() {{ echo "flock $*"; [ "$*" != "-n 9" ] || [ -z "${{BUSY:-}}" ]; }}
{setup}
{block}
connect_lock
if connect_lock_held; then echo has-lock; else echo no-lock; fi
"#
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        out.trim().replace('\n', ", ")
    }

    /// connect.sh takes one lock on every server, which the verifier's runs
    /// and Pyo's own timers and installer take too, and doesn't wait for
    /// itself under them.
    #[test]
    fn connect_takes_turns_with_other_pyo_setups_on_a_server() {
        assert_eq!(connect_lock(""), "flock -n 9, has-lock");
        // Something else runs (the verifier, Pyo's timer): it waits, and
        // says so.
        assert_eq!(
            connect_lock("BUSY=1"),
            "flock -n 9, Waiting for another Pyo setup on this server to finish., flock 9, has-lock"
        );
        // Run by Pyo's timer or installer, which hold it: theirs.
        assert_eq!(connect_lock("exec 9>\"$T/lock\""), "has-lock");
        // Not root (--uninstall with KUBECONFIG): /run isn't its to write.
        assert_eq!(connect_lock("ID=1000"), "no-lock");
        // Before --tailnet-only's exit and after --uninstall's.
        let at = CONNECT_SCRIPT.find("\n[ -n \"$TAILNET_ONLY\" ] || connect_lock\n").unwrap();
        assert!(CONNECT_SCRIPT.find("\n  uninstall\n  exit 0\n").unwrap() < at);
        assert!(at < CONNECT_SCRIPT.find("\nnvidia_gpu\n").unwrap());
    }

    /// The lines release-manifest.ts writes the verifier's release and
    /// SHA-256 on, in the connect.sh a release publishes.
    const VERIFY_PINS: [(&str, &str); 3] = [
        ("release", "PYO_VERIFY_RELEASE"),
        ("x86_64", "PYO_VERIFY_SHA256_X86_64"),
        ("aarch64", "PYO_VERIFY_SHA256_AARCH64"),
    ];

    fn verify_pin_line(name: &str, var: &str, value: &str) -> String {
        format!("\n{var}=\"{value}\" # pyo:verify-pin {name}\n")
    }

    /// `script` as a release publishes it: `pins` are the release, then the
    /// x86_64 and aarch64 verifiers' SHA-256.
    fn stamp_verifier(script: &str, pins: [&str; 3]) -> String {
        let mut s = script.to_owned();
        for ((name, var), value) in VERIFY_PINS.iter().zip(pins) {
            let empty = verify_pin_line(name, var, "");
            assert_eq!(s.matches(&empty).count(), 1, "{name}");
            s = s.replace(&empty, &verify_pin_line(name, var, value));
        }
        s
    }

    /// Pyo's own connect.sh has the pin lines empty, and the release's
    /// script fills in exactly those.
    #[test]
    fn the_release_pins_its_verifier_in_connect_sh() {
        let manifest = include_str!("../../../scripts/release-manifest.ts");
        for (name, var) in VERIFY_PINS {
            assert_eq!(CONNECT_SCRIPT.matches(&verify_pin_line(name, var, "")).count(), 1, "{name}");
            assert_eq!(CONNECT_SCRIPT.matches(&format!("# pyo:verify-pin {name}\n")).count(), 1);
            assert!(manifest.contains(&format!("[\"{name}\", \"{var}\"")), "{name}");
        }
        assert!(manifest.contains("`\\n${variable}=\"${value}\" # pyo:verify-pin ${name}\\n`"));
        // Only connect.sh is given them.
        for other in [SCRIPT, SANDBOX_SCRIPT, include_str!("../../../deploy/install.sh")] {
            assert!(!other.contains("pyo:verify-pin"));
        }
        // What the verifier's own release names: the same files, from the
        // same place.
        assert!(CONNECT_SCRIPT.contains(
            "\nPYO_VERIFY_RELEASES=https://github.com/upsetdev/pyo/releases/download\n"
        ));
        assert!(CONNECT_SCRIPT.contains("\"$PYO_VERIFY_RELEASES/v$PYO_VERIFY_RELEASE/pyo-verify-linux-$arch\""));
    }

    /// connect.sh's options, read by its own loop: AUTO_UPDATE, or the
    /// error.
    fn auto_update_arg(args: &str) -> String {
        let block = script_block("connect-args", CONNECT_SCRIPT);
        let (code, out, err) = bash(&format!(
            "set -euo pipefail\nset -- {args}\n{block}\necho \"[$AUTO_UPDATE] $API $TOKEN [$UNINSTALL]\"\n"
        ));
        if code == 0 { out.trim().to_owned() } else { format!("{code} {}", err.trim()) }
    }

    /// A machine on another tailnet leaves it for --tailnet's, once: run
    /// again on the new one, nothing changes. Without a key or --tailnet,
    /// nothing either.
    #[test]
    fn connect_switches_tailnet_only_when_asked() {
        let args = script_block("connect-args", CONNECT_SCRIPT);
        let switch = script_block("tailnet-switch", CONNECT_SCRIPT);
        let run = |args_line: &str, on: &str| {
            let (code, out, err) = bash(&format!(
                "set -euo pipefail\nset -- {args_line}\n{args}\n{switch}\n\
                 tailscale() {{ case \"$1\" in\n\
                   ip) echo 100.64.0.7 ;;\n\
                   status) printf '{{\\n  \"MagicDNSSuffix\": \"%s\",\\n}}\\n' '{on}' ;;\n\
                   logout) echo LOGGED-OUT ;;\n\
                 esac; }}\n\
                 tailnet_switch\n"
            ));
            assert_eq!(code, 0, "{err}");
            out.trim().to_owned()
        };
        let moving = "--tailnet-only --tailnet 'tail2.ts.net.' --tailscale-key 'tskey-auth-k'";
        assert_eq!(
            run(moving, "tail1.ts.net"),
            "Leaving tail1.ts.net for tail2.ts.net.\nLOGGED-OUT"
        );
        assert_eq!(run(moving, "tail2.ts.net"), "");
        assert_eq!(
            run(
                "--tailnet-only --tailscale-key 'tskey-auth-k'",
                "tail1.ts.net"
            ),
            ""
        );
        assert_eq!(
            run("--tailnet-only --tailnet 'tail2.ts.net'", "tail1.ts.net"),
            ""
        );
        // What Pyo gives to run.
        let c = tailnet_move_command(
            "https://pyo.example/install",
            "tskey-auth-k",
            "tail2.ts.net",
        );
        assert!(
            c.contains("--tailnet-only --tailnet 'tail2.ts.net' --tailscale-key 'tskey-auth-k'"),
            "{c}"
        );
    }

    #[test]
    fn connect_reads_the_auto_update_flags() {
        // As Pyo's commands give them: last, or before a mirror's --version.
        assert_eq!(auto_update_arg("--auto-update"), "[on]   []");
        assert_eq!(auto_update_arg("--no-auto-update"), "[off]   []");
        assert_eq!(auto_update_arg("--api 'https://pyo.example/' --token 'ab12' --auto-update"), "[on] https://pyo.example ab12 []");
        assert_eq!(
            auto_update_arg("--api 'https://pyo.example' --token 'ab12' --local-registry --no-auto-update --version '0.51.0'"),
            "[off] https://pyo.example ab12 []"
        );
        assert_eq!(auto_update_arg("--auto-update --version '0.51.0'"), "[on]   []");
        // Neither: nothing chosen in this run.
        assert_eq!(auto_update_arg(""), "[]   []");
        assert_eq!(auto_update_arg("--api 'https://pyo.example' --token 'ab12'"), "[] https://pyo.example ab12 []");
        // Both: the last one.
        assert_eq!(auto_update_arg("--auto-update --no-auto-update"), "[off]   []");
        assert_eq!(auto_update_arg("--no-auto-update --auto-update"), "[on]   []");
        assert_eq!(auto_update_arg("--uninstall --yes"), "[]   [1]");
        assert_eq!(auto_update_arg("--auto-updates"), "2 Unknown option: --auto-updates");
        assert_eq!(auto_update_arg("--auto-update=off"), "2 Unknown option: --auto-update=off");
    }

    /// A server for connect.sh's auto-update steps: its files under a
    /// directory, a release on "GitHub" with a stand-in verifier, and curl,
    /// systemctl, uname and id answered here. flock and sha256sum are real.
    struct VerifyHost {
        dir: std::path::PathBuf,
    }

    /// The x86_64 verifier's sum when a test wants one that matches nothing.
    const OTHER_SUM: &str = "00000000000000000000000000000000000000000000000000000000000000ff";

    impl VerifyHost {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pyo-verify-host-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for sub in ["bin", "etc/pyo", "units", "release", "lib"] {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
            VerifyHost { dir }
        }

        fn d(&self) -> String {
            self.dir.display().to_string()
        }

        /// A stand-in verifier that says it is `version`. It writes what it
        /// is asked to `log`, and says so when it was started with the
        /// lock's file.
        fn verifier(&self, version: &str) -> String {
            format!(
                r#"#!/usr/bin/env bash
T='{d}'
[ ! -e "/proc/$$/fd/9" ] || echo "$1 was started with file 9" >>"$T/log"
case "$1" in
  version) echo '{version}' ;;
  install)
    echo "{version} $*" >>"$T/log"
    [ ! -e "$T/install-fails" ] || exit 1
    [ "$0" -ef "$T/bin/pyo-verify" ] || cp "$0" "$T/bin/pyo-verify"
    ;;
  uninstall)
    echo "{version} uninstall" >>"$T/log"
    rm -f "$T/bin/pyo-verify"
    ;;
  run)
    exec 8>"$T/lock"
    if flock -n 8; then echo "run: took the lock"; else echo "run: skipped"; fi >>"$T/log"
    ;;
esac
"#,
                d = self.d()
            )
        }

        /// Puts `version`'s verifier in the release, as `file`. Its SHA-256.
        fn release(&self, file: &str, version: &str) -> String {
            use sha2::{Digest, Sha256};
            let text = self.verifier(version);
            std::fs::write(self.dir.join("release").join(file), &text).unwrap();
            hex::encode(Sha256::digest(text.as_bytes()))
        }

        /// The verifier this server already has.
        fn installed(&self, version: &str) {
            use std::os::unix::fs::PermissionsExt;
            let path = self.dir.join("bin/pyo-verify");
            std::fs::write(&path, self.verifier(version)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn has(&self, file: &str) -> bool {
            self.dir.join(file).exists()
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.dir.join(file)).unwrap_or_default()
        }

        /// What curl, systemctl and the verifier were asked, a line each.
        fn log(&self) -> Vec<String> {
            let log = self.read("log").replace(&self.d(), "");
            let _ = std::fs::remove_file(self.dir.join("log"));
            log.lines().map(str::to_owned).collect()
        }

        /// The blocks as connect.sh has them, with `pins` written as a
        /// release does (None: Pyo's own copy), then `steps`.
        fn bash(&self, pins: Option<[&str; 3]>, steps: &str) -> (i32, String, String) {
            let d = self.d();
            let script = match pins {
                Some(p) => stamp_verifier(CONNECT_SCRIPT, p),
                None => CONNECT_SCRIPT.to_owned(),
            };
            let progress = script_block("progress", &script);
            let lock = script_block("connect-lock", &script).replace("/run/pyo-connect.lock", &format!("{d}/lock"));
            let block = script_block("auto-update", &script)
                .replace("/etc/pyo/auto-update", &format!("{d}/etc/pyo/auto-update"))
                .replace("/usr/local/bin/pyo-verify", &format!("{d}/bin/pyo-verify"))
                .replace("/etc/systemd/system", &format!("{d}/units"))
                .replace("/usr/local/lib/pyo/connect-this-machine.sh", &format!("{d}/lib/connect-this-machine.sh"));
            let install = script_block("install-url", &script)
                .replace("/etc/pyo/install-url", &format!("{d}/etc/pyo/install-url"));
            bash(&format!(
                r#"set -euo pipefail
T='{d}'
AUTO_UPDATE=""
id() {{ echo 0; }}
uname() {{ echo "${{ARCH:-x86_64}}"; }}
sleep() {{ :; }}
systemctl() {{ echo "systemctl $*" >>"$T/log"; }}
curl() {{
  local out="" url=""
  while [ $# -gt 0 ]; do
    case "$1" in -o) out=$2; shift 2 ;; --proto | --max-time | --max-filesize) shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac
  done
  echo "curl $url" >>"$T/log"
  [ -f "$T/release/${{url##*/}}" ] || return 22
  cp "$T/release/${{url##*/}}" "$out"
}}
{progress}
{lock}
{block}
{install}
{steps}
"#
            ))
        }

        /// A connect run with `flag` ("on", "off" or none), as connect.sh
        /// orders the steps: what it printed, which has to end in
        /// "connected".
        fn connect(&self, pins: Option<[&str; 3]>, flag: &str) -> String {
            let (code, out, err) = self.bash(
                pins,
                &format!(
                    r#"AUTO_UPDATE={flag}
{CONNECT_STEPS}
echo connected"#
                ),
            );
            assert_eq!((code, err.as_str()), (0, ""), "{out}");
            let out = out.trim_end();
            assert!(out.ends_with("connected"), "{out}");
            out.trim_end_matches("connected").trim_end().replace('\n', " | ")
        }
    }

    impl Drop for VerifyHost {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    /// connect.sh's own lines for the lock and automatic updates, in the
    /// order it runs them, for Pyo at https://pyo.example.
    const CONNECT_STEPS: &str = r#"connect_lock
auto_update_choice
[ "$AUTO_UPDATE" != off ] || auto_update_off
[ "$AUTO_UPDATE" != on ] || auto_update_on https://pyo.example"#;

    /// CONNECT_STEPS are connect.sh's: the lock first, the choice once
    /// --tailnet-only has left, the verifier once the agent runs.
    #[test]
    fn connect_sets_up_automatic_updates_after_the_agent() {
        let at = |text: &str| CONNECT_SCRIPT.find(text).unwrap_or_else(|| panic!("no {text:?}"));
        let lock = at("\n[ -n \"$TAILNET_ONLY\" ] || connect_lock\n");
        let choice = at("\nauto_update_choice\n[ \"$AUTO_UPDATE\" != off ] || auto_update_off\n");
        let agent = at("k3s kubectl -n pyo-system rollout status deployment/pyo-agent");
        let on = at("\n[ \"$AUTO_UPDATE\" != on ] || auto_update_on \"$INSTALL_URL\"\n");
        let done = at("\nok_line \"Connected ");
        assert!(lock < choice && choice < agent && agent < on && on < done);
        assert!(at("  echo \"$(hostname) is on your tailnet.\"\n") < choice);
        for line in CONNECT_STEPS.lines() {
            let line = line.replace("https://pyo.example", "\"$INSTALL_URL\"").replace("connect_lock", "[ -n \"$TAILNET_ONLY\" ] || connect_lock");
            assert_eq!(CONNECT_SCRIPT.matches(&format!("\n{line}\n")).count(), 1, "{line}");
        }
        // Nothing about it shows sudo or a command to type.
        let block = script_block("auto-update", CONNECT_SCRIPT);
        let said: Vec<&str> = block
            .lines()
            .filter(|l| !l.starts_with('#') && l.to_lowercase().contains("automatic updates"))
            .collect();
        assert_eq!(said.len(), 9, "{said:?}");
        for line in said {
            assert!(!line.contains("sudo") && !line.contains(';') && !line.contains(" - "), "{line}");
        }
    }

    const SET_UP: &str = "→ Setting up automatic updates… | ✓ Set up automatic updates";

    #[test]
    fn a_released_connect_installs_its_pinned_verifier() {
        let h = VerifyHost::new("installs");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let pins = Some(["0.51.0", sum.as_str(), OTHER_SUM]);
        assert_eq!(h.connect(pins, "on"), SET_UP);
        assert_eq!(
            h.log(),
            [
                "curl https://github.com/upsetdev/pyo/releases/download/v0.51.0/pyo-verify-linux-x86_64",
                "0.51.0 install --url https://pyo.example",
            ]
        );
        assert_eq!(h.read("bin/pyo-verify"), h.verifier("0.51.0"));
        assert_eq!(h.read("etc/pyo/auto-update"), "on\n");
        // Nothing of the download stays.
        assert!(!h.has("bin/.pyo-verify.new"));
        // A release candidate's verifier takes release candidates.
        let h = VerifyHost::new("installs-rc");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0-rc.1");
        assert_eq!(h.connect(Some(["0.51.0-rc.1", sum.as_str(), OTHER_SUM]), "on"), SET_UP);
        assert_eq!(
            h.log(),
            [
                "curl https://github.com/upsetdev/pyo/releases/download/v0.51.0-rc.1/pyo-verify-linux-x86_64",
                "0.51.0-rc.1 install --url https://pyo.example --channel rc",
            ]
        );
    }

    #[test]
    fn connect_remembers_the_auto_update_choice() {
        let h = VerifyHost::new("remembers");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let pins = Some(["0.51.0", sum.as_str(), OTHER_SUM]);
        // Never asked for: nothing, and nothing kept.
        assert_eq!(h.connect(pins, ""), "");
        assert!(h.log().is_empty());
        assert!(!h.has("etc/pyo/auto-update") && !h.has("bin/pyo-verify"));
        // On, then the refresh command without arguments: still on. The
        // verifier it has is this release's, so nothing is downloaded.
        assert_eq!(h.connect(pins, "on"), SET_UP);
        h.log();
        assert_eq!(h.connect(pins, ""), SET_UP);
        assert_eq!(h.log(), ["0.51.0 install --url https://pyo.example"]);
        // Off, and it stays off without arguments.
        assert_eq!(h.connect(pins, "off"), "✓ Turned off automatic updates");
        assert_eq!(h.log(), ["0.51.0 uninstall"]);
        assert_eq!(h.read("etc/pyo/auto-update"), "off\n");
        assert_eq!(h.connect(pins, ""), "");
        assert!(h.log().is_empty());
        assert!(!h.has("bin/pyo-verify"));
        // And on again.
        assert_eq!(h.connect(pins, "on"), SET_UP);
        assert_eq!(h.read("etc/pyo/auto-update"), "on\n");
        // A file that says neither is no choice.
        std::fs::write(h.dir.join("etc/pyo/auto-update"), "maybe\n").unwrap();
        h.installed("0.51.0");
        h.log();
        assert_eq!(h.connect(pins, ""), "");
        assert!(h.log().is_empty());
    }

    #[test]
    fn connect_takes_the_verifier_for_the_server_s_processor() {
        for (machine, file, arm) in [
            ("x86_64", "pyo-verify-linux-x86_64", false),
            ("amd64", "pyo-verify-linux-x86_64", false),
            ("aarch64", "pyo-verify-linux-aarch64", true),
            ("arm64", "pyo-verify-linux-aarch64", true),
        ] {
            let h = VerifyHost::new(&format!("arch-{machine}"));
            let sum = h.release(file, "0.51.0");
            // The other processor's sum matches nothing here.
            let pins = if arm { ["0.51.0", OTHER_SUM, sum.as_str()] } else { ["0.51.0", sum.as_str(), OTHER_SUM] };
            let (code, out, err) = h.bash(Some(pins), &format!("ARCH={machine} AUTO_UPDATE=on\n{CONNECT_STEPS}\n"));
            assert_eq!((code, err.as_str()), (0, ""), "{machine}: {out}");
            assert!(out.ends_with("✓ Set up automatic updates\n"), "{machine}: {out}");
            assert_eq!(
                h.log(),
                [
                    format!("curl https://github.com/upsetdev/pyo/releases/download/v0.51.0/{file}"),
                    "0.51.0 install --url https://pyo.example".to_owned(),
                ],
                "{machine}"
            );
        }
        // A processor the release has no verifier for.
        let h = VerifyHost::new("arch-other");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let (code, out, _) = h.bash(Some(["0.51.0", sum.as_str(), OTHER_SUM]), &format!("ARCH=riscv64 AUTO_UPDATE=on\n{CONNECT_STEPS}\necho connected"));
        assert_eq!(code, 0);
        assert_eq!(out, "● Automatic updates aren't available here. They stay off on this server.\nconnected\n");
        assert!(h.log().is_empty());
    }

    /// Pyo's own copy of connect.sh (a dev build, a self-hosted Pyo) has no
    /// pins: the flag installs nothing, and the connect goes on.
    #[test]
    fn a_connect_script_without_pins_installs_no_verifier() {
        const NEEDS: &str = "● Automatic updates aren't available here. They stay off on this server.";
        let h = VerifyHost::new("unpinned");
        h.release("pyo-verify-linux-x86_64", "0.51.0");
        assert_eq!(h.connect(None, "on"), NEEDS);
        assert!(h.log().is_empty(), "nothing is downloaded");
        assert!(!h.has("bin/pyo-verify"));
        // Half a pin is none: a release without a sum, a sum that isn't one,
        // a release that isn't one.
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        for pins in [
            ["0.51.0", "", ""],
            ["", sum.as_str(), sum.as_str()],
            ["0.51.0", "abc", "abc"],
            ["0.51", sum.as_str(), sum.as_str()],
            ["latest", sum.as_str(), sum.as_str()],
            ["0.51.0/../x", sum.as_str(), sum.as_str()],
        ] {
            assert_eq!(h.connect(Some(pins), "on"), NEEDS, "{pins:?}");
            assert!(h.log().is_empty(), "{pins:?}");
        }
        // Off works without pins, and a verifier a release set up stays
        // as it is, quietly.
        h.installed("0.51.0");
        assert_eq!(h.connect(None, "on"), "");
        assert!(h.log().is_empty());
        assert_eq!(h.connect(None, "off"), "✓ Turned off automatic updates");
        // The server Pyo itself runs on never gets one, whatever the flag,
        // and remembers none.
        let h = VerifyHost::new("own-server");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        std::fs::write(h.dir.join("lib/connect-this-machine.sh"), "").unwrap();
        for flag in ["on", "off", ""] {
            assert_eq!(h.connect(Some(["0.51.0", sum.as_str(), OTHER_SUM]), flag), "", "{flag}");
        }
        assert!(h.log().is_empty());
        assert!(!h.has("etc/pyo/auto-update") && !h.has("bin/pyo-verify"));
    }

    #[test]
    fn a_verifier_that_fails_its_checksum_is_not_installed() {
        let h = VerifyHost::new("mismatch");
        h.release("pyo-verify-linux-x86_64", "0.51.0");
        // What the release pinned is another file.
        assert_eq!(
            h.connect(Some(["0.51.0", OTHER_SUM, OTHER_SUM]), "on"),
            "→ Setting up automatic updates… | ● Automatic updates aren't set up. The download didn't match its checksum."
        );
        // Downloaded once, never run, nothing left.
        assert_eq!(h.log(), ["curl https://github.com/upsetdev/pyo/releases/download/v0.51.0/pyo-verify-linux-x86_64"]);
        assert!(!h.has("bin/pyo-verify") && !h.has("bin/.pyo-verify.new"));
        // The choice stays: the next run tries again.
        assert_eq!(h.read("etc/pyo/auto-update"), "on\n");
    }

    #[test]
    fn a_failed_verifier_download_or_setup_does_not_stop_the_connect() {
        // The release has no such file (or GitHub doesn't answer).
        let h = VerifyHost::new("download");
        assert_eq!(
            h.connect(Some(["0.51.0", OTHER_SUM, OTHER_SUM]), "on"),
            "→ Setting up automatic updates… | ● Automatic updates aren't set up. The download failed."
        );
        assert_eq!(h.log().len(), 3, "three tries");
        assert!(!h.has("bin/pyo-verify") && !h.has("bin/.pyo-verify.new"));
        // The verifier's own install fails.
        let h = VerifyHost::new("install-fails");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        std::fs::write(h.dir.join("install-fails"), "").unwrap();
        assert_eq!(
            h.connect(Some(["0.51.0", sum.as_str(), OTHER_SUM]), "on"),
            "→ Setting up automatic updates… | ● Automatic updates couldn't be set up on this server."
        );
        assert!(!h.has("bin/pyo-verify") && !h.has("bin/.pyo-verify.new"));
        // Without the lock (no flock here), nothing is set up: the
        // verifier's install counts on connect.sh holding it.
        let h = VerifyHost::new("no-lock");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let (code, out, err) = h.bash(
            Some(["0.51.0", sum.as_str(), OTHER_SUM]),
            "AUTO_UPDATE=on\nauto_update_choice\nauto_update_on https://pyo.example\necho connected",
        );
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, "● Automatic updates couldn't be set up on this server.\nconnected\n");
        assert!(h.log().is_empty());
    }

    #[test]
    fn no_auto_update_removes_the_verifier() {
        let h = VerifyHost::new("off");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let pins = Some(["0.51.0", sum.as_str(), OTHER_SUM]);
        // None here: nothing to say.
        assert_eq!(h.connect(pins, "off"), "");
        assert!(h.log().is_empty());
        assert_eq!(h.read("etc/pyo/auto-update"), "off\n");
        h.installed("0.50.0");
        assert_eq!(h.connect(pins, "off"), "✓ Turned off automatic updates");
        assert_eq!(h.log(), ["0.50.0 uninstall"]);
        assert!(!h.has("bin/pyo-verify"));
        // A verifier that doesn't run here: its timer and files go anyway.
        for file in ["bin/pyo-verify", "bin/pyo-verify.previous", "units/pyo-verify.timer", "units/pyo-verify.service"] {
            std::fs::write(h.dir.join(file), "not a program").unwrap();
        }
        assert_eq!(h.connect(pins, "off"), "✓ Turned off automatic updates");
        assert_eq!(h.log(), ["systemctl disable --now pyo-verify.timer", "systemctl daemon-reload"]);
        for file in ["bin/pyo-verify", "bin/pyo-verify.previous", "units/pyo-verify.timer", "units/pyo-verify.service"] {
            assert!(!h.has(file), "{file}");
        }
    }

    #[test]
    fn connect_never_moves_the_verifier_backwards() {
        let h = VerifyHost::new("backwards");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let pins = Some(["0.51.0", sum.as_str(), OTHER_SUM]);
        // The verifier moved on to a later release: kept, and set up again.
        for later in ["0.51.1", "0.52.0", "1.0.0", "0.52.0-rc.1"] {
            h.installed(later);
            assert_eq!(h.connect(pins, "on"), SET_UP, "{later}");
            assert_eq!(h.log(), [format!("{later} install --url https://pyo.example")]);
            assert_eq!(h.read("bin/pyo-verify"), h.verifier(later));
        }
        // An earlier one, a release candidate of this release, or one that
        // doesn't say: replaced by this release's.
        for earlier in ["0.50.9", "0.5.5", "0.51.0-rc.9", "", "dev"] {
            h.installed(earlier);
            assert_eq!(h.connect(pins, "on"), SET_UP, "{earlier}");
            assert_eq!(h.log().last().unwrap(), "0.51.0 install --url https://pyo.example", "{earlier}");
            assert_eq!(h.read("bin/pyo-verify"), h.verifier("0.51.0"));
        }
        let (code, out, err) = h.bash(
            None,
            r#"for pair in "0.51.0 0.52.0" "0.51.9 0.52.0" "0.9.0 0.10.0" "0.51.0-rc.1 0.51.0" "0.51.0-rc.2 0.51.0-rc.10" "0.50.9 0.51.0-rc.1"; do
  set -- $pair
  verify_older "$1" "$2" || echo "$1 isn't older than $2"
  ! verify_older "$2" "$1" || echo "$2 is older than $1"
  ! verify_older "$1" "$1" || echo "$1 is older than itself"
done
! verify_older 0.51.0 "" && ! verify_older "" 0.51.0 && ! verify_older 0.51.0 v0.52.0 && ! verify_older 0.51.0 0.52 || echo "not versions"
"#,
        );
        assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));
    }

    /// With the real flock: a verifier run skips while connect.sh has the
    /// lock, connect.sh waits for a run that has it, the verifier connect.sh
    /// starts never gets the lock's file, and the lock goes with the script.
    #[test]
    fn connect_and_the_verifier_never_overlap_or_wait_for_each_other() {
        if bash("command -v flock").0 != 0 {
            return;
        }
        let h = VerifyHost::new("turns");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let pins = Some(["0.51.0", sum.as_str(), OTHER_SUM]);
        // The timer fires in the middle of a connect: that run skips. The
        // connect's own steps (install, then off's uninstall) don't take
        // the lock, so they don't wait for the script that holds it.
        let (code, out, err) = h.bash(
            pins,
            &format!(
                r#"AUTO_UPDATE=on
{CONNECT_STEPS}
"$T/bin/pyo-verify" run 9>&-
AUTO_UPDATE=off
auto_update_choice
auto_update_off
connect_lock
echo connected
"#
            ),
        );
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert_eq!(
            out,
            "→ Setting up automatic updates…\n✓ Set up automatic updates\n✓ Turned off automatic updates\nconnected\n"
        );
        assert_eq!(
            h.log(),
            [
                "curl https://github.com/upsetdev/pyo/releases/download/v0.51.0/pyo-verify-linux-x86_64",
                "0.51.0 install --url https://pyo.example",
                "run: skipped",
                "0.51.0 uninstall",
            ],
            "no line says a verifier was started with file 9"
        );
        // The script ended: the next run has the lock to itself.
        h.installed("0.51.0");
        let (code, _, err) = bash(&format!("'{}/bin/pyo-verify' run", h.d()));
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(h.log(), ["run: took the lock"]);
        // A run has the lock: the connect waits for it, then goes on.
        let (code, out, err) = h.bash(
            pins,
            r#"( exec 8>"$T/lock"; flock 8; : >"$T/running"; command sleep 0.5; echo "run: done" >>"$T/log" ) &
until [ -e "$T/running" ]; do command sleep 0.05; done
connect_lock
echo "connect: has the lock" >>"$T/log"
wait
"#,
        );
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert_eq!(out, "Waiting for another Pyo setup on this server to finish.\n");
        assert_eq!(h.log(), ["run: done", "connect: has the lock"]);
    }

    /// connect.sh's uninstall() on a server whose cluster answers nothing
    /// much: what it says and asks the verifier.
    fn uninstall_with_verifier(h: &VerifyHost, vars: &str) -> (i32, String, String) {
        let start = CONNECT_SCRIPT.find("\nuninstall() {\n").unwrap();
        let end = CONNECT_SCRIPT[start..].find("\n}\nif [ -n \"$UNINSTALL\" ]; then\n").unwrap();
        let uninstall = &CONNECT_SCRIPT[start..start + end + 3];
        h.bash(
            None,
            &format!(
                r#"unset KUBECONFIG
KEEP_DATA="" YES=1 DRY_RUN="" WITH_PYO=1
k3s() {{ return 0; }}
{vars}
{uninstall}
uninstall
"#
            ),
        )
    }

    #[test]
    fn uninstall_removes_the_verifier_from_the_server() {
        let h = VerifyHost::new("uninstall");
        h.installed("0.51.0");
        std::fs::write(h.dir.join("etc/pyo/auto-update"), "on\n").unwrap();
        std::fs::write(h.dir.join("etc/pyo/install-url"), "https://cloud.example\n").unwrap();
        // A dry run only says so.
        let (code, out, err) = uninstall_with_verifier(&h, "DRY_RUN=1");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.contains("\n  - automatic updates on this server\n"), "{out}");
        assert!(out.contains("\n  would remove automatic updates from this server\n"), "{out}");
        assert!(h.log().is_empty());
        assert!(h.has("bin/pyo-verify") && h.has("etc/pyo/auto-update"));
        // For real: before the agent stops, under the lock.
        let (code, out, err) = uninstall_with_verifier(&h, "");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        let removing = out.find("\nRemoving automatic updates…\n").expect(&out);
        assert!(removing < out.find("\nStopping the Pyo agent…\n").unwrap(), "{out}");
        assert_eq!(h.log(), ["0.51.0 uninstall"]);
        assert!(!h.has("bin/pyo-verify") && !h.has("etc/pyo/auto-update"));
        assert!(!h.has("etc/pyo/install-url"));
        // No verifier here: not a word about it, and the choice goes.
        std::fs::write(h.dir.join("etc/pyo/auto-update"), "off\n").unwrap();
        let (code, out, err) = uninstall_with_verifier(&h, "");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(!out.contains("automatic updates"), "{out}");
        assert!(h.log().is_empty());
        assert!(!h.has("etc/pyo/auto-update"));
    }

    /// connect.sh's own lines from its options to the verifier, in its
    /// order: the lock, a refresh reads the agent's settings, the install
    /// address is kept, then the manifest (the agent-manifest block) and
    /// the verifier come from it.
    const INSTALL_STEPS: [&str; 6] = [
        "[ -n \"$TAILNET_ONLY\" ] || connect_lock",
        "if [ -z \"$TAILNET_ONLY\" ] && [ -z \"$API\" ] && [ -z \"$TOKEN\" ] && command -v k3s >/dev/null; then\n  agent_settings\n  [ -z \"$API\" ] || echo \"Updating this cluster's Pyo agent.\"\nfi",
        "auto_update_choice\n[ \"$AUTO_UPDATE\" != off ] || auto_update_off\ninstall_url_keep",
        "API=$(tailnet_url \"$API\")\nINSTALL_URL=$(tailnet_url \"${INSTALL_URL:-$API}\")",
        "AGENT_MANIFEST",
        "[ \"$AUTO_UPDATE\" != on ] || auto_update_on \"$INSTALL_URL\"",
    ];

    /// A connect.sh run on `h` with `args`, from its options to the
    /// verifier ([`INSTALL_STEPS`]). The agent already here reaches Pyo at
    /// `agent` (its Secret's PYO_API_URL), with the token ab12. Pyo answers
    /// at every address but https://agent.cloud.example, which takes only
    /// the agents' own requests (Pyo Cloud's Cloudflare rule) and answers
    /// 403. What it printed. `h.log()` has what k3s and curl were asked.
    fn connect_run(h: &VerifyHost, pins: Option<[&str; 3]>, args: &str, agent: &str) -> (i32, String, String) {
        let script = match pins {
            Some(p) => stamp_verifier(CONNECT_SCRIPT, p),
            None => CONNECT_SCRIPT.to_owned(),
        };
        let steps = INSTALL_STEPS
            .join("\n")
            .replace("AGENT_MANIFEST", &script_block("agent-manifest", &script));
        h.bash(
            pins,
            &format!(
                r#"AGENT_API='{agent}' MANIFEST="$T/manifest" WEB_QUERY=""
k3s() {{
  echo "k3s $*" >>"$T/log"
  case "$*" in
    "kubectl -n pyo-system get secret pyo-agent -o jsonpath={{.data.PYO_API_URL}}") printf %s "$AGENT_API" | base64 ;;
    "kubectl -n pyo-system get secret pyo-agent -o jsonpath={{.data.PYO_AGENT_TOKEN}}") printf %s ab12 | base64 ;;
    *) return 1 ;;
  esac
}}
curl() {{
  local out="" url=""
  while [ $# -gt 0 ]; do
    case "$1" in -o) out=$2; shift 2 ;; -w | --proto | --max-time | --max-filesize) shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac
  done
  echo "curl $url" >>"$T/log"
  case "$url" in
    https://agent.cloud.example/*) echo 403; return 0 ;;
    */install/agent.yaml\?token=ab12) echo manifest >"$out"; echo 200; return 0 ;;
    */install/agent.yaml*) echo 404; return 0 ;;
  esac
  [ -f "$T/release/${{url##*/}}" ] || return 22
  cp "$T/release/${{url##*/}}" "$out"
}}
{tailnet}
set -- {args}
{connect_args}
{steps}
echo "agent at $API, installs from $INSTALL_URL"
"#,
                tailnet = script_block("tailnet-host", &script),
                connect_args = script_block("connect-args", &script),
            ),
        )
    }

    /// The steps [`connect_run`] runs are connect.sh's, in its order, and
    /// every install file comes from the install address.
    #[test]
    fn connect_gets_its_install_files_from_the_install_address() {
        let at = |text: &str| {
            assert_eq!(CONNECT_SCRIPT.matches(text).count(), 1, "{text}");
            CONNECT_SCRIPT.find(text).unwrap()
        };
        let mut last = 0;
        for step in INSTALL_STEPS {
            let here = match step {
                "AGENT_MANIFEST" => at("\n# >>> pyo:agent-manifest "),
                _ => at(&format!("{step}\n")),
            };
            assert!(last < here, "{step}");
            last = here;
        }
        // Checked as --api is, before anything is set up.
        let api = at("\n[ -n \"$TAILNET_ONLY\" ] || api_ok \"$API\" || ");
        let install = at("\n[ -n \"$TAILNET_ONLY\" ] || [ -z \"$INSTALL_URL\" ] || api_ok \"$INSTALL_URL\" || { echo \"Pyo's address must be https:// ($INSTALL_URL isn't on a private network or the tailnet).\" >&2; exit 2; }\n");
        assert!(api < install && install < at("\n[ \"$(id -u)\" = 0 ] || "));
        // The gateway's CRDs too. Nothing else is fetched from Pyo.
        assert!(CONNECT_SCRIPT.contains(
            "curl -fsS --max-time 60 -o \"$CRDS\" \"$INSTALL_URL/install/gateway-crds.yaml$q\" && break\n"
        ));
        let fetches: Vec<&str> = CONNECT_SCRIPT
            .lines()
            .filter(|l| l.contains("/install/") && !l.trim_start().starts_with('#'))
            .collect();
        assert_eq!(fetches.len(), 2, "{fetches:?}");
        assert!(fetches.iter().all(|l| l.contains("\"$INSTALL_URL/install/")), "{fetches:?}");
        assert!(!CONNECT_SCRIPT.contains("$API/"));
        // --uninstall drops the kept address.
        assert!(CONNECT_SCRIPT.contains("rm -f \"$AUTO_UPDATE_FILE\" \"$INSTALL_URL_FILE\"\n"));
    }

    #[test]
    fn connect_reads_the_install_address() {
        let h = VerifyHost::new("install-flag");
        let (code, out, err) = connect_run(&h, None, "--api 'https://pyo.example' --token 'ab12' --install-url 'https://cloud.example/'", "");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert_eq!(out, "agent at https://pyo.example, installs from https://cloud.example\n");
        let (code, _, err) = connect_run(&h, None, "--install-url", "");
        assert_ne!(code, 0, "{err}");
    }

    const AGENT: &str = "https://agent.cloud.example";

    const SECRET_READS: [&str; 2] = [
        "k3s kubectl -n pyo-system get secret pyo-agent -o jsonpath={.data.PYO_API_URL}",
        "k3s kubectl -n pyo-system get secret pyo-agent -o jsonpath={.data.PYO_AGENT_TOKEN}",
    ];

    /// Pyo Cloud's refresh command: the manifest and the verifier from Pyo's
    /// public address, while the agent keeps its own (only read here, the
    /// manifest Pyo hands out names it). Later runs without the flag (a
    /// command copied before it) find the address kept.
    #[test]
    fn a_hosted_refresh_installs_from_the_public_address() {
        let h = VerifyHost::new("hosted-refresh");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let pins = Some(["0.51.0", sum.as_str(), OTHER_SUM]);
        let (code, out, err) = connect_run(&h, pins, "--install-url 'https://cloud.example' --auto-update", AGENT);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.starts_with("Updating this cluster's Pyo agent.\n"), "{out}");
        assert!(out.contains("Set up automatic updates"), "{out}");
        assert!(
            out.ends_with("agent at https://agent.cloud.example, installs from https://cloud.example\n"),
            "{out}"
        );
        assert_eq!(
            h.log(),
            [
                SECRET_READS[0],
                SECRET_READS[1],
                "curl https://cloud.example/install/agent.yaml?token=ab12",
                "curl https://github.com/upsetdev/pyo/releases/download/v0.51.0/pyo-verify-linux-x86_64",
                "0.51.0 install --url https://cloud.example",
            ]
        );
        assert_eq!(h.read("manifest"), "manifest\n");
        assert_eq!(h.read("etc/pyo/install-url"), "https://cloud.example\n");

        // A refresh command copied before --install-url: the kept address,
        // and the verifier (already this release's) is given it again.
        for args in ["--auto-update", ""] {
            let (code, out, err) = connect_run(&h, pins, args, AGENT);
            assert_eq!((code, err.as_str()), (0, ""), "{out}");
            assert!(out.ends_with("installs from https://cloud.example\n"), "{out}");
            assert_eq!(
                h.log(),
                [
                    SECRET_READS[0],
                    SECRET_READS[1],
                    "curl https://cloud.example/install/agent.yaml?token=ab12",
                    "0.51.0 install --url https://cloud.example",
                ]
            );
        }
        assert_eq!(h.read("etc/pyo/install-url"), "https://cloud.example\n");
    }

    /// A refresh command copied before --install-url, on a server that
    /// never ran one with it: the agent's address turns the manifest away,
    /// so it stops before anything is installed, and says to copy the
    /// command again.
    #[test]
    fn an_old_hosted_refresh_asks_for_the_new_command() {
        let h = VerifyHost::new("old-refresh");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        for args in ["", "--auto-update"] {
            let (code, out, err) = connect_run(&h, Some(["0.51.0", sum.as_str(), OTHER_SUM]), args, AGENT);
            assert_eq!(code, 1, "{out}");
            assert_eq!(
                err,
                "Pyo at https://agent.cloud.example answered 403 for the agent manifest.\nCopy the command from Pyo again and run that.\n"
            );
            assert_eq!(
                h.log(),
                [SECRET_READS[0], SECRET_READS[1], "curl https://agent.cloud.example/install/agent.yaml?token=ab12"]
            );
            assert!(!h.has("bin/pyo-verify") && !h.has("etc/pyo/install-url"));
        }
        // A first connect isn't told to copy it again.
        let (code, _, err) = connect_run(&h, None, "--api 'https://agent.cloud.example' --token 'ab12'", "");
        assert_eq!(code, 1);
        assert_eq!(err, "Pyo at https://agent.cloud.example answered 403 for the agent manifest.\n");
    }

    /// The first connect: everything from --api, as before. Pyo Cloud's
    /// command also names it as the install address, which is kept. One
    /// without it (self-hosted) drops a kept one: one address serves both.
    #[test]
    fn a_first_connect_installs_from_its_api() {
        let h = VerifyHost::new("first-connect");
        let sum = h.release("pyo-verify-linux-x86_64", "0.51.0");
        let (code, out, err) = connect_run(
            &h,
            Some(["0.51.0", sum.as_str(), OTHER_SUM]),
            "--api 'https://cloud.example' --token 'ab12' --install-url 'https://cloud.example' --auto-update",
            "",
        );
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(!out.contains("Updating"), "{out}");
        assert_eq!(
            h.log(),
            [
                "curl https://cloud.example/install/agent.yaml?token=ab12",
                "curl https://github.com/upsetdev/pyo/releases/download/v0.51.0/pyo-verify-linux-x86_64",
                "0.51.0 install --url https://cloud.example",
            ]
        );
        assert_eq!(h.read("etc/pyo/install-url"), "https://cloud.example\n");
        // Self-hosted, or a connect command from before: nothing kept.
        let h = VerifyHost::new("first-connect-self-hosted");
        std::fs::write(h.dir.join("etc/pyo/install-url"), "https://cloud.example\n").unwrap();
        let (code, out, err) = connect_run(&h, None, "--api 'https://pyo.example/' --token 'ab12'", "");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert_eq!(out, "agent at https://pyo.example, installs from https://pyo.example\n");
        assert_eq!(h.log(), ["curl https://pyo.example/install/agent.yaml?token=ab12"]);
        assert!(!h.has("etc/pyo/install-url"));
    }

    /// Self-hosted refreshes, as before: everything from the agent's
    /// address. Pyo's own server too (connect-this-machine.sh), from Pyo
    /// on it, whatever a file says.
    #[test]
    fn a_self_hosted_refresh_installs_from_the_agent_s_address() {
        let h = VerifyHost::new("self-hosted-refresh");
        for (agent, from) in [
            ("https://pyo.example/", "https://pyo.example"),
            ("http://100.64.0.1:8000", "http://100.64.0.1:8000"),
        ] {
            let (code, out, err) = connect_run(&h, None, "", agent);
            assert_eq!((code, err.as_str()), (0, ""), "{out}");
            assert_eq!(out, format!("Updating this cluster's Pyo agent.\nagent at {from}, installs from {from}\n"));
            assert_eq!(
                h.log(),
                [
                    SECRET_READS[0].to_owned(),
                    SECRET_READS[1].to_owned(),
                    format!("curl {from}/install/agent.yaml?token=ab12"),
                ]
            );
            assert!(!h.has("etc/pyo/install-url"));
        }
        // Pyo's own server: its agent reaches Pyo in the cluster.
        std::fs::write(h.dir.join("lib/connect-this-machine.sh"), "").unwrap();
        std::fs::write(h.dir.join("etc/pyo/install-url"), "https://cloud.example\n").unwrap();
        let (code, out, err) = connect_run(&h, None, "", "http://pyo.pyo-system.svc:8000");
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert_eq!(
            out,
            "Updating this cluster's Pyo agent.\nagent at http://127.0.0.1:8000, installs from http://127.0.0.1:8000\n"
        );
        assert_eq!(
            h.log().last().map(String::as_str),
            Some("curl http://127.0.0.1:8000/install/agent.yaml?token=ab12")
        );
        assert_eq!(h.read("etc/pyo/install-url"), "https://cloud.example\n");
    }

    /// A kept address that isn't one is left out.
    #[test]
    fn a_kept_install_address_has_to_be_one() {
        let h = VerifyHost::new("kept-garbage");
        for kept in ["", "pyo.example\n", "ftp://pyo.example\n", "https://\n"] {
            std::fs::write(h.dir.join("etc/pyo/install-url"), kept).unwrap();
            let (code, out, err) = connect_run(&h, None, "", "https://pyo.example");
            assert_eq!((code, err.as_str()), (0, ""), "{out}");
            assert!(out.ends_with("installs from https://pyo.example\n"), "{kept:?} {out}");
            h.log();
        }
        std::fs::write(h.dir.join("etc/pyo/install-url"), "https://cloud.example/\nmore\n").unwrap();
        let (code, out, err) = connect_run(&h, None, "", AGENT);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.ends_with("installs from https://cloud.example\n"), "{out}");
    }
}
