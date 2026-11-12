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
    let ip = rest.rsplit('.').next().unwrap_or("").replace('-', ".");
    let octets: Vec<u8> = ip.split('.').filter_map(|o| o.parse().ok()).collect();
    match octets[..] {
        [10, ..] | [127, ..] | [192, 168, ..] => true,
        [172, b, ..] => (16..=31).contains(&b),
        [100, b, ..] => (64..=127).contains(&b),
        _ => false,
    }
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

/// The cluster's ingress address, for the free domain and the DNS record.
fn ingress(t: &Target) -> Option<String> {
    let c = t
        .state
        .cluster(t.service()["clusterId"].as_str().unwrap_or(""))?;
    c["capabilities"]["loadBalancerIPs"]
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_owned)
}

fn ingress_v4(t: &Target) -> Option<String> {
    let c = t
        .state
        .cluster(t.service()["clusterId"].as_str().unwrap_or(""))?;
    c["capabilities"]["loadBalancerIPs"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|i| i.split('.').count() == 4 && i.split('.').all(|p| p.parse::<u8>().is_ok()))
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
                port_of(s, d["portName"].as_str().unwrap_or("")),
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
    }
    Ok(())
}

pub async fn add(
    api: &Api,
    service: Option<&str>,
    hostname: Option<String>,
    free: bool,
    port: Option<String>,
    wait: bool,
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
            let ip = ingress_v4(&t).context("The cluster has no ingress IP yet, so there's no free domain for it. Use your own hostname.")?;
            let ns = t
                .state
                .project(s["projectId"].as_str().unwrap_or(""))
                .and_then(|p| p["namespace"].as_str())
                .unwrap_or("app");
            let label: String = format!("{}-{ns}", s["name"].as_str().unwrap_or(""))
                .chars()
                .take(63)
                .collect();
            format!(
                "{}.{}.nip.io",
                label.trim_end_matches('-'),
                ip.replace('.', "-")
            )
        }
        None => bail!("Give a hostname, or --free for a free domain."),
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
    let i = match &port {
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
    let port_name = ports[i]["name"].clone();
    let private = private_host(&host);
    let domain = json!({
        "id": new_id("dom"), "hostname": host, "portName": port_name, "status": "pending_dns",
        "tls": "pending", "tlsMode": "auto", "redirectHttps": !private, "createdAt": now_ms(),
    });
    let id = domain["id"].as_str().unwrap_or("").to_owned();
    s["domains"]
        .as_array_mut()
        .context("service has no domains list")?
        .push(domain);
    api.op("saveService", json!({ "service": s })).await?;
    ui::ok(&format!("Added {host}"));
    if !deployed(&s) {
        println!("{}", ui::dim("Live after the first deploy: pyo deploy"));
        return Ok(());
    }
    if !is_free(&host) {
        println!("Create this DNS record at your DNS provider:");
        match ingress(&t) {
            Some(ip) => println!(
                "  {}  {host}  {ip}",
                if ip.contains(':') { "AAAA" } else { "A" }
            ),
            None => println!(
                "  CNAME  {host}  {}",
                ui::dim("(the cluster's ingress address)")
            ),
        }
    }
    if !wait {
        println!(
            "{}",
            ui::dim("Checked automatically. See it: pyo domain list")
        );
        return Ok(());
    }
    follow(api, &t.id, &id, &host).await
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

pub async fn remove(api: &Api, service: Option<&str>, hostname: &str, yes: bool) -> Result<()> {
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
    fn private_hosts() {
        assert!(private_host("x.internal"));
        assert!(private_host("app-ns.100-64-0-10.nip.io"));
        assert!(private_host("app-ns.192-168-1-2.nip.io"));
        assert!(!private_host("app-ns.95-216-1-2.nip.io"));
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
        assert_eq!(https(&d("active", "issued", "auto")), "Let's Encrypt");
        assert_eq!(https(&d("active", "disabled", "off")), "off");
    }
}
