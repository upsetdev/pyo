//! `pyo domain`: a service's domains. Adding or removing one saves the
//! service the way the dashboard does, and it applies in place, without a
//! redeploy (a service never deployed gets it with its first deploy).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    api::{Api, new_id, now_ms},
    service::{self, Target},
    ui,
};

/// Only a tailnet, LAN or this machine reaches it, so Let's Encrypt can't
/// (the API's `private_host`).
fn private_host(host: &str) -> bool {
    let h = host.to_lowercase();
    if h == "localhost"
        || [
            ".localhost",
            ".local",
            ".internal",
            ".lan",
            ".home.arpa",
            ".test",
        ]
        .iter()
        .any(|s| h.ends_with(s))
    {
        return true;
    }
    let Some(rest) = h
        .strip_suffix(".nip.io")
        .or_else(|| h.strip_suffix(".sslip.io"))
    else {
        return false;
    };
    // "app.100.64.0.2", or the dashed "app.100-64-0-2".
    let labels: Vec<&str> = rest.rsplit('.').take(4).collect();
    let ip = if labels.len() == 4 && labels.iter().all(|l| l.parse::<u8>().is_ok()) {
        format!("{}.{}.{}.{}", labels[3], labels[2], labels[1], labels[0])
    } else {
        labels.first().copied().unwrap_or("").replace('-', ".")
    };
    let Ok(ip) = ip.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let [a, b, ..] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || (a == 100 && (64..=127).contains(&b))
}

fn is_free(host: &str) -> bool {
    host.ends_with(".nip.io")
}

/// "app.example.com": DNS labels and a top-level domain of letters.
fn valid_hostname(h: &str) -> bool {
    let labels: Vec<&str> = h.split('.').collect();
    let label = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    let tld = labels.last().copied().unwrap_or("");
    h.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(label)
        && (2..=63).contains(&tld.len())
        && tld.chars().all(|c| c.is_ascii_lowercase())
}

/// A cluster address the API picked (never a private one where it means
/// public): `key` "publicIp" (for DNS records) or "freeDomainIp" (where
/// its free domains point: its public or Tailscale IP).
fn cluster_ip(t: &Target, key: &str) -> Option<String> {
    t.state
        .cluster(t.service()["clusterId"].as_str().unwrap_or(""))?[key]
        .as_str()
        .map(str::to_owned)
}

#[derive(PartialEq)]
enum Tone {
    Ok,
    Busy,
    Broken,
}

/// One status for a domain, as the dashboard shows it.
fn state(d: &Value, deployed: bool) -> (Tone, String) {
    if !deployed {
        return (Tone::Busy, "Live after first deploy".into());
    }
    let host = d["hostname"].as_str().unwrap_or("");
    if d["status"] != json!("active") {
        // Tailnet domains have no DNS to wait for: Pyo sets them up.
        if pyo_proto::system::is_tailnet_host(host) {
            return if d["status"] == json!("error") {
                (Tone::Broken, "Tailscale failed".into())
            } else {
                (Tone::Busy, "Setting up on Tailscale…".into())
            };
        }
        // Through the Cloudflare Tunnel: Pyo says when Cloudflare points it there.
        if d["tunnel"].is_string() {
            return (Tone::Busy, "Moving to the tunnel…".into());
        }
        if is_free(host) {
            return (Tone::Busy, "Checking DNS…".into());
        }
        let tone = if d["status"] == json!("error") {
            Tone::Broken
        } else {
            Tone::Busy
        };
        let checked = d["dnsCheckedAt"]
            .as_i64()
            .map(|at| format!(" · checked {}", ui::ago(at)))
            .unwrap_or_default();
        return (tone, format!("Waiting for DNS{checked}"));
    }
    if d["tlsMode"] == json!("off") || d["tlsMode"] == json!("custom") {
        return (Tone::Ok, "Live".into());
    }
    match d["tls"].as_str() {
        Some("failed") => (Tone::Broken, "Certificate failed".into()),
        Some("issued") => (Tone::Ok, "Live".into()),
        _ => (Tone::Busy, "Issuing certificate…".into()),
    }
}

fn https(d: &Value) -> &'static str {
    let mode = d["tlsMode"].as_str().unwrap_or("auto");
    match (mode, d["tls"].as_str().unwrap_or("")) {
        ("off", _) => "off",
        ("custom", _) => "own certificate",
        (_, "failed") => "failed",
        (_, "issued")
            if mode == "selfsigned"
                || (mode == "auto" && private_host(d["hostname"].as_str().unwrap_or(""))) =>
        {
            "self-signed"
        }
        (_, "issued") => "Let's Encrypt",
        _ => "pending",
    }
}

fn paint(tone: &Tone, s: &str) -> String {
    match tone {
        Tone::Ok => ui::green(s),
        Tone::Busy => s.to_owned(),
        Tone::Broken => ui::red(s),
    }
}

fn deployed(s: &Value) -> bool {
    !s["currentDeploymentId"].is_null()
}

/// "http (3000)": the port a domain routes to.
fn port_of(s: &Value, name: &str) -> String {
    match s["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == json!(name))
    {
        Some(p) => format!("{name} ({})", p["containerPort"]),
        None => name.to_owned(),
    }
}

pub async fn list(api: &Api, service: Option<&str>) -> Result<()> {
    let t = service::target(api, service).await?;
    let s = t.service();
    let domains = s["domains"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if domains.is_empty() {
        println!("No domains. Add one: pyo domain add <hostname>");
        return Ok(());
    }
    let rows: Vec<[String; 4]> = domains
        .iter()
        .map(|d| {
            let (tone, text) = state(d, deployed(s));
            [
                d["hostname"].as_str().unwrap_or("").to_owned(),
                paint(&tone, &text),
                https(d).to_owned(),
                match d["portName"].as_str().unwrap_or("") {
                    "" => "redirect".to_owned(),
                    p => port_of(s, p),
                },
            ]
        })
        .collect();
    let width = |i: usize, head: &str| {
        rows.iter()
            .map(|r| r[i].chars().count())
            .chain([head.len()])
            .max()
            .unwrap_or(0)
    };
    let (w0, w2) = (width(0, "HOSTNAME"), width(2, "HTTPS"));
    // The status is colored: pad by its visible text.
    let w1 = domains
        .iter()
        .map(|d| state(d, deployed(s)).1.chars().count())
        .chain([6])
        .max()
        .unwrap_or(0);
    println!(
        "{}",
        ui::dim(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  PORT",
            "HOSTNAME", "STATUS", "HTTPS"
        ))
    );
    for (r, d) in rows.iter().zip(domains) {
        let pad = w1 - state(d, deployed(s)).1.chars().count();
        println!(
            "{:<w0$}  {}{}  {:<w2$}  {}",
            r[0],
            r[1],
            " ".repeat(pad),
            r[2],
            r[3]
        );
        if let Some(e) = d["tlsError"]
            .as_str()
            .filter(|_| d["tls"] == json!("failed"))
        {
            println!("  {}", ui::red(e));
        }
        for r in d["redirects"].as_array().into_iter().flatten() {
            println!(
                "  {}",
                ui::dim(&format!(
                    "{} → {} ({})",
                    r["path"].as_str().unwrap_or("/"),
                    redirect_target(r),
                    r["status"].as_u64().unwrap_or(301)
                ))
            );
        }
        for l in header_lines(&d["headers"]) {
            println!("  {}", ui::dim(&l));
        }
    }
    Ok(())
}

/// What `pyo domain add` does about the DNS record in Cloudflare.
pub struct Dns {
    /// Create it when a connected zone covers the hostname (the default).
    pub cloudflare: bool,
    pub proxied: bool,
}

#[allow(clippy::too_many_arguments)]
pub async fn add(
    api: &Api,
    service: Option<&str>,
    hostname: Option<String>,
    free: bool,
    port: Option<String>,
    wait: bool,
    dns: Dns,
    redirect: Option<Value>,
) -> Result<()> {
    let t = service::target(api, service).await?;
    let mut s = t.service().clone();
    let host = match hostname {
        Some(h) => {
            let h = h.trim().trim_end_matches('.').to_lowercase();
            if !valid_hostname(&h) {
                bail!("\"{h}\" isn't a hostname, like app.example.com.");
            }
            h
        }
        None if free => {
            let tunnel = cluster_ip(&t, "traffic").as_deref() == Some("tunnel");
            // Its public or Tailscale IP's nip.io name, or in tunnel mode a
            // name under the tunnel's default domain.
            let suffix = match cluster_ip(&t, "freeDomainSuffix") {
                Some(s) => s,
                None if tunnel => bail!("The cluster's Cloudflare Tunnel has no default domain, so there's no tunnel domain for it. Pick a default domain first, or use your own hostname."),
                None => {
                    let ip = cluster_ip(&t, "freeDomainIp").context("The cluster has no address for free domains (its public or Tailscale IP, as its settings say), so there's no free domain for it. Use your own hostname.")?;
                    format!("{ip}.nip.io")
                }
            };
            let ns = t
                .state
                .project(s["projectId"].as_str().unwrap_or(""))
                .and_then(|p| p["namespace"].as_str())
                .unwrap_or("app");
            // One DNS label: 63 characters at most.
            let label: String = format!("{}-{ns}", s["name"].as_str().unwrap_or(""))
                .chars()
                .take(63)
                .collect();
            format!("{}.{suffix}", label.trim_end_matches('-'))
        }
        None => bail!("Give a hostname, or --free for a free or tunnel domain."),
    };
    if let Some(other) = t.state.services().iter().find(|a| {
        a["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|d| d["hostname"] == json!(host))
    }) {
        bail!("{host} is already on {}.", t.state.path_of(other));
    }
    let private = private_host(&host);
    let mut domain = json!({
        "id": new_id("dom"), "hostname": host, "portName": "", "status": "pending_dns",
        "tls": "pending", "tlsMode": "auto", "redirectHttps": !private, "createdAt": now_ms(),
    });
    match redirect {
        // Only a redirect: no port.
        Some(r) => domain["redirects"] = json!([r]),
        None => domain["portName"] = http_port(&mut s, &t, port.as_deref())?,
    }
    let id = domain["id"].as_str().unwrap_or("").to_owned();
    s["domains"]
        .as_array_mut()
        .context("service has no domains list")?
        .push(domain);
    api.op("saveService", json!({ "service": s })).await?;
    ui::ok(&format!("Added {host}"));
    after_add(api, &t, &s, &host, &id, wait, dns).await
}

/// The HTTP port a new domain routes to (the first, or the one named),
/// made public. Its name.
fn http_port(s: &mut Value, t: &Target, port: Option<&str>) -> Result<Value> {
    let ports = s["ports"]
        .as_array_mut()
        .context("service has no ports list")?;
    let http: Vec<usize> = (0..ports.len())
        .filter(|&i| ports[i]["protocol"] == json!("HTTP"))
        .collect();
    if http.is_empty() {
        bail!(
            "{} has no HTTP port. Domains need one.",
            t.state.path_of(t.service())
        );
    }
    let i = match port {
        None => http[0],
        Some(p) => *http
            .iter()
            .find(|&&i| {
                ports[i]["name"] == json!(p)
                    || ports[i]["containerPort"]
                        .as_i64()
                        .is_some_and(|n| p.parse() == Ok(n))
            })
            .with_context(|| {
                let names: Vec<String> = http
                    .iter()
                    .map(|&i| {
                        format!(
                            "{} ({})",
                            ports[i]["name"].as_str().unwrap_or(""),
                            ports[i]["containerPort"]
                        )
                    })
                    .collect();
                format!("No HTTP port {p}. Its HTTP ports: {}.", names.join(", "))
            })?,
    };
    ports[i]["exposure"] = json!("http");
    Ok(ports[i]["name"].clone())
}

/// After a domain is added: its DNS record (or the tunnel), then what's
/// left to do, or its progress with `wait`.
async fn after_add(
    api: &Api,
    t: &Target,
    s: &Value,
    host: &str,
    id: &str,
    wait: bool,
    dns: Dns,
) -> Result<()> {
    let host = host.to_owned();
    // Its record in the team's Cloudflare account, when a zone there covers it.
    let team_id = s["teamId"].as_str().unwrap_or("");
    let in_cloudflare = !is_free(&host)
        && crate::cloudflare::zone_for(&t.state.0, team_id, &host)
            .is_some_and(|z| z["canEdit"] == json!(true));
    // Through the cluster's Cloudflare Tunnel: Pyo makes the switch. A name
    // under the tunnel's default domain gets no record of its own (its
    // wildcard has it), unless a record on that name is in the way.
    let tunnel = cluster_ip(t, "traffic").as_deref() == Some("tunnel");
    let free_tunnel = tunnel
        && cluster_ip(t, "freeDomainSuffix")
            .is_some_and(|s| host.strip_suffix(&format!(".{s}")).is_some_and(|l| !l.contains('.')));
    let made = if tunnel && in_cloudflare && dns.cloudflare {
        crate::cloudflare::domain_tunnel(api, Some(&t.id), &host, true, false, true)
            .await
            .map_err(|e| eprintln!("{}", ui::red(&e.to_string())))
            .is_ok()
    } else if free_tunnel {
        true
    } else if in_cloudflare && dns.cloudflare {
        crate::cloudflare::create_record(api, &t.id, id, &host, dns.proxied, false)
            .await
            .map_err(|e| eprintln!("{}", ui::red(&e.to_string())))
            .is_ok()
    } else {
        false
    };
    if !deployed(s) {
        println!("{}", ui::dim("Live after the first deploy: pyo deploy"));
        return Ok(());
    }
    let tailnet = pyo_proto::system::is_tailnet_host(&host);
    if tailnet {
        println!("{}", ui::dim(TAILNET_NOTE));
    } else if !is_free(&host) && !made {
        println!("Create this DNS record at your DNS provider:");
        println!(
            "  {}",
            dns_record(&host, cluster_ip(t, "publicIp").as_deref())
        );
    }
    if !wait {
        println!(
            "{}",
            ui::dim(if tailnet {
                "See it: pyo domain list"
            } else {
                "Checked automatically. See it: pyo domain list"
            })
        );
        return Ok(());
    }
    follow(api, &t.id, id, &host).await
}

/// A redirect of `path` to `to`: a hostname, a hostname and path, or a
/// path ("https://" left off).
pub fn redirect_to(path: &str, to: &str, status: Option<&str>, keep_path: bool) -> Result<Value> {
    let to = to
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let (to_host, to_path) = match to.find('/') {
        Some(i) => (&to[..i], &to[i..]),
        None => (to, ""),
    };
    let to_host = to_host.trim_end_matches('.').to_lowercase();
    if !to_host.is_empty() && !valid_hostname(&to_host) {
        bail!("\"{to_host}\" isn't a hostname, like example.com.");
    }
    if to_host.is_empty() && to_path.is_empty() {
        bail!("Say where to: --to example.com or --to /new.");
    }
    Ok(json!({
        "path": path, "toHost": to_host, "toPath": to_path,
        "status": status.and_then(|s| s.parse::<u16>().ok()).unwrap_or(301),
        "keepPath": keep_path,
    }))
}

/// "example.com/old": the hostname and the path ("/" without one).
fn split_source(from: &str) -> (String, String) {
    let from = from
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let (host, path) = match from.find('/') {
        Some(i) => (&from[..i], &from[i..]),
        None => (from, "/"),
    };
    let path = match path.trim_end_matches('/') {
        "" => "/",
        p => p,
    };
    (host.trim_end_matches('.').to_lowercase(), path.to_owned())
}

/// `pyo domain redirect`: a redirect added, replaced or removed. A hostname
/// the service doesn't have yet becomes a domain that only redirects.
pub async fn redirect(
    api: &Api,
    service: Option<&str>,
    from: &str,
    to: Option<&str>,
    status: Option<&str>,
    keep_path: bool,
    remove: bool,
) -> Result<()> {
    let (host, path) = split_source(from);
    if !valid_hostname(&host) {
        bail!("\"{host}\" isn't a hostname, like www.example.com.");
    }
    let t = service::target(api, service).await?;
    let mut s = t.service().clone();
    let has = s["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["hostname"] == json!(host));
    if !has {
        if remove {
            bail!("{host} isn't one of {}'s domains.", t.state.path_of(t.service()));
        }
        if path != "/" {
            bail!("Add {host} first: pyo domain add {host}");
        }
        let r = redirect_to("/", to.unwrap_or(""), status, keep_path)?;
        let dns = Dns { cloudflare: true, proxied: false };
        return add(api, service, Some(host), false, None, false, dns, Some(r)).await;
    }
    let d = s["domains"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .find(|d| d["hostname"] == json!(host))
        .context("no such domain")?;
    let mut list: Vec<Value> = d["redirects"].as_array().cloned().unwrap_or_default();
    let at = list.iter().position(|r| r["path"] == json!(path));
    let shown = if path == "/" { host.clone() } else { format!("{host}{path}") };
    if remove {
        let Some(i) = at else {
            bail!("{shown} has no redirect.");
        };
        if d["portName"].as_str().unwrap_or("").is_empty() && list.len() == 1 {
            bail!("{host} only redirects. Remove the domain instead: pyo domain remove {host}");
        }
        list.remove(i);
    } else {
        let r = redirect_to(&path, to.unwrap_or(""), status, keep_path)?;
        match at {
            Some(i) => list[i] = r,
            None => list.push(r),
        }
    }
    d["redirects"] = json!(list);
    api.op("saveService", json!({ "service": s })).await?;
    ui::ok(&if remove {
        format!("Removed the redirect of {shown}")
    } else {
        format!("{shown} redirects to {}", to.unwrap_or("").trim())
    });
    Ok(())
}

/// "Name: value", as `--set` takes it.
fn header_pair(s: &str) -> Result<(String, String)> {
    let (name, value) = s
        .split_once(':')
        .with_context(|| format!("\"{s}\" isn't \"Name: value\"."))?;
    Ok((name.trim().to_owned(), value.trim().to_owned()))
}

/// `pyo domain header`: header rules changed, or listed without changes.
pub async fn header(
    api: &Api,
    service: Option<&str>,
    hostname: &str,
    set: &[String],
    remove: &[String],
    unset: &[String],
    request: bool,
) -> Result<()> {
    let host = hostname.trim().trim_end_matches('.').to_lowercase();
    let t = service::target(api, service).await?;
    let mut s = t.service().clone();
    let d = s["domains"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .find(|d| d["hostname"] == json!(host))
        .with_context(|| format!("{host} isn't one of this service's domains."))?;
    if set.is_empty() && remove.is_empty() && unset.is_empty() {
        let lines = header_lines(&d["headers"]);
        if lines.is_empty() {
            println!("No header rules. Add one: pyo domain header {host} --set 'Name: value'");
        }
        for l in lines {
            println!("{l}");
        }
        return Ok(());
    }
    let side = if request { "request" } else { "response" };
    let mut rules = d["headers"].clone();
    if !rules.is_object() {
        rules = json!({});
    }
    let mut sets: Vec<Value> = rules[side]["set"].as_array().cloned().unwrap_or_default();
    let mut removes: Vec<Value> = rules[side]["remove"].as_array().cloned().unwrap_or_default();
    let same = |a: &Value, b: &str| a.as_str().is_some_and(|a| a.eq_ignore_ascii_case(b));
    let gone: Vec<&str> = unset
        .iter()
        .chain(remove)
        .map(String::as_str)
        .chain(set.iter().filter_map(|x| x.split_once(':').map(|p| p.0.trim())))
        .collect();
    sets.retain(|x| !gone.iter().any(|g| same(&x["name"], g)));
    removes.retain(|x| !gone.iter().any(|g| same(x, g)));
    for x in set {
        let (name, value) = header_pair(x)?;
        sets.push(json!({ "name": name, "value": value }));
    }
    for r in remove {
        removes.push(json!(r.trim()));
    }
    rules[side] = json!({ "set": sets, "remove": removes });
    d["headers"] = rules;
    api.op("saveService", json!({ "service": s })).await?;
    ui::ok(&format!("Saved the header rules of {host}"));
    Ok(())
}

/// A domain's header rules, a line each: "response  set  Name: value".
fn header_lines(h: &Value) -> Vec<String> {
    let mut out = vec![];
    for side in ["response", "request"] {
        for x in h[side]["set"].as_array().into_iter().flatten() {
            out.push(format!(
                "{side:<8}  set     {}: {}",
                x["name"].as_str().unwrap_or(""),
                x["value"].as_str().unwrap_or("")
            ));
        }
        for x in h[side]["remove"].as_array().into_iter().flatten() {
            out.push(format!("{side:<8}  remove  {}", x.as_str().unwrap_or("")));
        }
    }
    out
}

/// Where a domain's redirect goes: "example.com/new", "/new".
fn redirect_target(r: &Value) -> String {
    format!(
        "{}{}",
        r["toHost"].as_str().unwrap_or(""),
        r["toPath"].as_str().unwrap_or("")
    )
}

/// A tailnet name's DNS is Tailscale's: there's no record to create.
const TAILNET_NOTE: &str = "Tailscale names need no DNS record. Pyo sets it up on your tailnet.";

/// The DNS record that points `host` at the cluster's public address `ip`
/// (a CNAME to it when the address isn't known).
fn dns_record(host: &str, ip: Option<&str>) -> String {
    match ip {
        Some(ip) => format!(
            "{}  {host}  {ip}",
            if ip.contains(':') { "AAAA" } else { "A" }
        ),
        None => format!("CNAME  {host}  {}", ui::dim("(the cluster's public address)")),
    }
}

/// Poll until the domain is live (or its certificate failed), checking
/// its DNS now and then. Gives up after 15 minutes.
async fn follow(api: &Api, service_id: &str, domain_id: &str, host: &str) -> Result<()> {
    println!(
        "{}",
        ui::dim("Waiting for it to go live (Ctrl-C stops waiting)…")
    );
    let started = now_ms();
    let mut last = String::new();
    for n in 0u32.. {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let snap = api.state().await?;
        let s = snap.service(service_id)?;
        let Some(d) = s["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["id"] == json!(domain_id))
        else {
            bail!("{host} was removed.")
        };
        let (tone, text) = state(d, deployed(s));
        let text = text.split(" · ").next().unwrap_or("").to_owned();
        if text != last {
            println!("{} {text}", ui::bold("→"));
            last = text;
        }
        match tone {
            Tone::Ok => {
                ui::ok(&format!(
                    "Live: {}://{host}",
                    if d["tlsMode"] == json!("off") {
                        "http"
                    } else {
                        "https"
                    }
                ));
                return Ok(());
            }
            Tone::Broken if d["tls"] == json!("failed") => bail!(
                "The certificate for {host} failed: {}",
                d["tlsError"].as_str().unwrap_or("see pyo domain list")
            ),
            _ => {}
        }
        // The DNS watcher checks on its own; this checks sooner.
        if n % 5 == 4 && d["status"] != json!("active") {
            let _ = api
                .op(
                    "verifyDomain",
                    json!({ "serviceId": service_id, "domainId": domain_id }),
                )
                .await;
        }
        if now_ms() - started > 15 * 60_000 {
            bail!("{host} isn't live after 15 minutes. It keeps checking: pyo domain list");
        }
    }
    unreachable!()
}

pub async fn remove(
    api: &Api,
    service: Option<&str>,
    hostname: &str,
    yes: bool,
    keep_dns_record: bool,
) -> Result<()> {
    let t = service::target(api, service).await?;
    let mut s = t.service().clone();
    let host = hostname.trim().trim_end_matches('.').to_lowercase();
    let name = s["name"].as_str().unwrap_or("").to_owned();
    let domains = s["domains"]
        .as_array_mut()
        .context("service has no domains list")?;
    let Some(i) = domains.iter().position(|d| d["hostname"] == json!(host)) else {
        if let Some(other) = t.state.services().iter().find(|a| {
            a["domains"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|d| d["hostname"] == json!(host))
        }) {
            bail!(
                "{host} is on {}: pyo domain remove {host} -s {}",
                t.state.path_of(other),
                t.state.path_of(other)
            );
        }
        bail!(
            "{host} isn't one of {}'s domains.",
            t.state.path_of(t.service())
        );
    };
    if !yes {
        let a = ui::ask(&format!(
            "Remove {host}? It stops serving {name} right away. [y/N] "
        ))?;
        if !a.eq_ignore_ascii_case("y") {
            bail!("Not removed.");
        }
    }
    // The record Pyo made in Cloudflare goes first, unless it's kept.
    let d = domains[i].clone();
    if d["dnsRecord"]["mine"] == json!(true) && !keep_dns_record {
        api.op(
            "deleteDnsRecord",
            json!({ "serviceId": t.id, "domainId": d["id"] }),
        )
        .await?;
        ui::ok(&format!("Deleted the DNS record of {host} in Cloudflare"));
    }
    let domains = s["domains"]
        .as_array_mut()
        .context("service has no domains list")?;
    domains.remove(i);
    // A port no domain routes to anymore isn't public.
    let used: Vec<Value> = s["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| d["portName"].clone())
        .collect();
    for p in s["ports"].as_array_mut().into_iter().flatten() {
        if p["exposure"] == json!("http") && !used.contains(&p["name"]) {
            p["exposure"] = json!("internal");
        }
    }
    api.op("saveService", json!({ "service": s })).await?;
    ui::ok(&format!("Removed {host}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_as_typed() {
        assert_eq!(split_source("www.example.com"), ("www.example.com".into(), "/".into()));
        assert_eq!(split_source("https://Example.com/old/"), ("example.com".into(), "/old".into()));
        let r = redirect_to("/", "https://example.com", None, true).unwrap();
        assert_eq!(r, json!({ "path": "/", "toHost": "example.com", "toPath": "", "status": 301, "keepPath": true }));
        let r = redirect_to("/old", "/new", Some("302"), false).unwrap();
        assert_eq!((r["toHost"].as_str(), r["toPath"].as_str(), r["status"].as_u64()), (Some(""), Some("/new"), Some(302)));
        assert!(redirect_to("/", "", None, true).is_err());
        assert_eq!(redirect_target(&json!({ "toHost": "example.com", "toPath": "/new" })), "example.com/new");
        assert_eq!(header_pair("X-Frame-Options: DENY").unwrap(), ("X-Frame-Options".into(), "DENY".into()));
        assert!(header_pair("nope").is_err());
        let lines = header_lines(&json!({ "response": { "set": [{ "name": "A", "value": "1" }], "remove": ["B"] } }));
        assert_eq!(lines, ["response  set     A: 1", "response  remove  B"]);
    }

    #[test]
    fn hostnames() {
        assert!(valid_hostname("app.example.com"));
        assert!(valid_hostname("a-b.c.io"));
        assert!(!valid_hostname("localhost"));
        assert!(!valid_hostname("-a.com"));
        assert!(!valid_hostname("a.c0m"));
        assert!(!valid_hostname("a..com"));
        assert!(!valid_hostname("App.com"));
    }

    #[test]
    fn records_to_create() {
        assert_eq!(
            dns_record("a.example.com", Some("203.0.113.7")),
            "A  a.example.com  203.0.113.7"
        );
        assert_eq!(
            dns_record("a.example.com", Some("2001:db8::1")),
            "AAAA  a.example.com  2001:db8::1"
        );
        assert!(dns_record("a.example.com", None).starts_with("CNAME  a.example.com"));
        // Tailscale names get the note instead of a record.
        assert!(pyo_proto::system::is_tailnet_host("web.tail1234.ts.net"));
        assert!(!TAILNET_NOTE.contains(';'));
    }

    #[test]
    fn private_hosts() {
        assert!(private_host("x.internal"));
        assert!(private_host("app-ns.100-64-0-10.nip.io"));
        assert!(private_host("app-ns.192-168-1-2.nip.io"));
        assert!(!private_host("app-ns.95-216-1-2.nip.io"));
        assert!(private_host("app-ns.100.64.0.2.nip.io"));
        assert!(private_host("app-ns.10.3.8.13.nip.io"));
        assert!(!private_host("app-ns.203.0.113.31.nip.io"));
        assert!(valid_hostname("app-ns.203.0.113.31.nip.io"));
        assert!(!private_host("app.example.com"));
    }

    #[test]
    fn states() {
        let d = |status: &str, tls: &str, mode: &str| json!({ "hostname": "a.example.com", "status": status, "tls": tls, "tlsMode": mode });
        assert_eq!(
            state(&d("pending_dns", "pending", "auto"), true).1,
            "Waiting for DNS"
        );
        assert_eq!(
            state(&d("active", "pending", "auto"), true).1,
            "Issuing certificate…"
        );
        assert_eq!(state(&d("active", "issued", "auto"), true).1, "Live");
        assert_eq!(state(&d("active", "disabled", "off"), true).1, "Live");
        assert_eq!(
            state(&d("active", "failed", "auto"), true).1,
            "Certificate failed"
        );
        assert_eq!(
            state(&d("active", "issued", "auto"), false).1,
            "Live after first deploy"
        );
        let tn = |status: &str| json!({ "hostname": "blog.tail1.ts.net", "status": status, "tls": "pending", "tlsMode": "auto" });
        assert_eq!(
            state(&tn("pending_dns"), true).1,
            "Setting up on Tailscale…"
        );
        // A tunnel domain doesn't wait for the cluster's DNS.
        let mut through = d("pending_dns", "pending", "auto");
        through["tunnel"] = json!("switching");
        assert_eq!(state(&through, true).1, "Moving to the tunnel…");
        let failed = state(&tn("error"), true);
        assert!(failed.0 == Tone::Broken && failed.1 == "Tailscale failed");
        assert_eq!(state(&tn("active"), true).1, "Issuing certificate…");
        assert_eq!(https(&d("active", "issued", "auto")), "Let's Encrypt");
        assert_eq!(https(&d("active", "disabled", "off")), "off");
    }
}
