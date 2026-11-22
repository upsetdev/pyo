//! `pyo cloudflare`: the team's Cloudflare account, which Pyo creates the
//! DNS records of domains with. Team owners and admins connect, test and
//! remove it. The token is read from a file or `CLOUDFLARE_API_TOKEN`,
//! never from an argument (it would stay in the shell's history).

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{api::Api, ui};

/// The team's id, from a slug or id (the first team without one).
async fn team(api: &Api, team: Option<&str>) -> Result<String> {
    let state = api.state().await?;
    let teams = state.teams();
    let t = match team {
        Some(t) => teams
            .iter()
            .find(|x| x["slug"] == json!(t) || x["id"] == json!(t))
            .with_context(|| format!("No team {t} among yours"))?,
        None => teams.first().context("You aren't in any team")?,
    };
    Ok(t["id"].as_str().unwrap_or("").to_owned())
}

/// The token: from `file` ("-" for stdin), else `env`.
fn read_token(file: Option<&str>, env: Option<String>) -> Result<String> {
    let t = match file {
        Some("-") => {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s
        }
        Some(f) => std::fs::read_to_string(f).with_context(|| format!("Can't read {f}"))?,
        None => env.unwrap_or_default(),
    };
    let t = t.trim().to_owned();
    if t.is_empty() {
        bail!(
            "Give the token in a file (--token-file, - for stdin) or in CLOUDFLARE_API_TOKEN. It needs Zone:Read and DNS:Edit, and for Cloudflare Tunnel also Cloudflare Tunnel:Edit and SSL and Certificates:Edit."
        );
    }
    Ok(t)
}

fn print_zones(zones: &Value) {
    let zones = zones.as_array().map(Vec::as_slice).unwrap_or(&[]);
    if zones.is_empty() {
        println!("No zones.");
        return;
    }
    let w = zones
        .iter()
        .map(|z| z["name"].as_str().unwrap_or("").len())
        .chain([4])
        .max()
        .unwrap_or(4);
    println!("{}", ui::dim(&format!("{:<w$}  {:<10}  TUNNEL", "ZONE", "DNS")));
    for z in zones {
        let name = z["name"].as_str().unwrap_or("");
        let can = z["canEdit"] == json!(true);
        let dns = if can {
            ui::green(&format!("{:<10}", "can edit"))
        } else {
            ui::red(&format!(
                "needs {}",
                z["missing"].as_str().unwrap_or("DNS:Edit")
            ))
        };
        let tunnel = if !can {
            String::new()
        } else {
            tunnel_column(&z["tunnel"])
        };
        println!("{name:<w$}  {dns}  {tunnel}");
    }
}

/// A zone's tunnel state: ready, DNS only, or the one permission it lacks.
fn tunnel_column(t: &Value) -> String {
    let missing: Vec<&str> = t["missing"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if t["ready"] == json!(true) {
        ui::green("ready")
    } else if missing.len() == 1 {
        ui::yellow(&format!("needs {}", missing[0]))
    } else {
        ui::dim("DNS only")
    }
}

fn print_problems(v: &Value) {
    for p in v["problems"].as_array().into_iter().flatten() {
        println!("{}", ui::yellow(p.as_str().unwrap_or("")));
    }
}

pub async fn connect(
    api: &Api,
    team_arg: Option<&str>,
    file: Option<&str>,
    env: Option<String>,
) -> Result<()> {
    let token = read_token(file, env)?;
    let team_id = team(api, team_arg).await?;
    let v = api
        .op(
            "connectCloudflare",
            json!({ "teamId": team_id, "token": token }),
        )
        .await?;
    ui::ok("Cloudflare connected");
    print_zones(&v["zones"]);
    print_problems(&v);
    Ok(())
}

pub async fn test(api: &Api, team_arg: Option<&str>) -> Result<()> {
    let team_id = team(api, team_arg).await?;
    let v = api
        .op("testCloudflare", json!({ "teamId": team_id }))
        .await?;
    if v["ok"] == json!(true) {
        ui::ok("The token works");
    }
    print_zones(&v["zones"]);
    print_problems(&v);
    Ok(())
}

pub async fn zones(api: &Api, team_arg: Option<&str>) -> Result<()> {
    let team_id = team(api, team_arg).await?;
    let v = api.get(&format!("/cloudflare?team={team_id}")).await?;
    if v["connected"] != json!(true) {
        println!("Cloudflare isn't connected. Connect it: pyo cloudflare connect");
        return Ok(());
    }
    print_zones(&v["zones"]);
    Ok(())
}

pub async fn remove(api: &Api, team_arg: Option<&str>, yes: bool) -> Result<()> {
    let team_id = team(api, team_arg).await?;
    if !yes {
        let a = ui::ask("Remove the Cloudflare token? Records Pyo made stay. [y/N] ")?;
        if !a.eq_ignore_ascii_case("y") {
            bail!("Not removed.");
        }
    }
    api.op("removeCloudflare", json!({ "teamId": team_id }))
        .await?;
    ui::ok("Cloudflare token removed");
    Ok(())
}

/// The connected zone `host` is in, from the snapshot: the longest one.
pub fn zone_for<'a>(state: &'a Value, team_id: &str, host: &str) -> Option<&'a Value> {
    let host = host.trim_end_matches('.').to_lowercase();
    state["cloudflare"]
        .as_array()?
        .iter()
        .find(|c| c["teamId"] == json!(team_id))?["zones"]
        .as_array()?
        .iter()
        .filter(|z| {
            let n = z["name"].as_str().unwrap_or("");
            !n.is_empty() && (host == n || host.ends_with(&format!(".{n}")))
        })
        .max_by_key(|z| z["name"].as_str().unwrap_or("").len())
}

/// Ask Pyo to create a domain's record, and say how it went.
pub async fn create_record(
    api: &Api,
    service_id: &str,
    domain_id: &str,
    host: &str,
    proxied: bool,
    replace: bool,
) -> Result<()> {
    let v = api
        .op(
            "createDnsRecord",
            json!({ "serviceId": service_id, "domainId": domain_id, "proxied": proxied, "replace": replace }),
        )
        .await?;
    let rec = format!(
        "{} {}",
        v["type"].as_str().unwrap_or("A"),
        v["content"].as_str().unwrap_or("")
    );
    match v["status"].as_str().unwrap_or("") {
        "created" => {
            ui::ok(&format!("Created {rec} for {host} in Cloudflare"));
            if v["proxy"] == json!(true) && v["proxied"] != json!(true) {
                println!(
                    "{}",
                    ui::dim("Proxied by Cloudflare once its certificate is issued.")
                );
            }
        }
        "exists" => ui::ok(&format!(
            "{host} already points to the cluster in Cloudflare"
        )),
        "conflict" => {
            println!("{}", ui::yellow("Cloudflare already has a record for it:"));
            for r in v["existing"].as_array().into_iter().flatten() {
                println!(
                    "  {}  {}  {}",
                    r["type"].as_str().unwrap_or(""),
                    r["name"].as_str().unwrap_or(""),
                    r["content"].as_str().unwrap_or("")
                );
            }
            println!("Replace it with {rec} (owners and admins): pyo domain dns {host} --replace");
        }
        _ => bail!(
            "Cloudflare: {}",
            v["error"].as_str().unwrap_or("the record wasn't created")
        ),
    }
    Ok(())
}

/// `pyo domain dns`: create (or retry) a domain's record.
pub async fn domain_dns(
    api: &Api,
    service: Option<&str>,
    hostname: &str,
    proxied: bool,
    replace: bool,
) -> Result<()> {
    let t = crate::service::target(api, service).await?;
    let host = hostname.trim().trim_end_matches('.').to_lowercase();
    let d = t.service()["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["hostname"] == json!(host))
        .with_context(|| {
            format!(
                "{host} isn't one of {}'s domains.",
                t.state.path_of(t.service())
            )
        })?;
    let proxied = proxied || d["dnsRecord"]["proxy"] == json!(true);
    create_record(
        api,
        &t.id,
        d["id"].as_str().unwrap_or(""),
        &host,
        proxied,
        replace,
    )
    .await
}

// ── Cloudflare Tunnel ──────────────────────────────────────────────────────

/// The cluster (name or id, else the team's default) a tunnel command is about.
async fn cluster_of(api: &Api, team: Option<&str>, cluster: Option<&str>) -> Result<(String, String)> {
    let (_, id) = crate::compose::team_and_cluster(api, team, cluster).await?;
    let state = api.state().await?;
    let name = state
        .cluster(&id)
        .and_then(|c| c["name"].as_str())
        .unwrap_or(&id)
        .to_owned();
    Ok((id, name))
}

/// A tunnel's state in one line, as the dashboard says it.
pub fn tunnel_line(t: &Value) -> String {
    let n = t["connectors"].as_array().map_or(0, Vec::len);
    let ready = t["connectors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["ready"] == json!(true))
        .count();
    let colos: Vec<&str> = t["colos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match t["health"].as_str().unwrap_or("unknown") {
        "connected" if colos.is_empty() => format!("Tunnel connected · {ready} connectors"),
        "connected" => format!("Tunnel connected · {ready} connectors · {}", colos.join(", ")),
        "degraded" => format!("Tunnel degraded · {ready} of {n} connectors"),
        "down" => "Tunnel down. Its websites don't answer.".into(),
        "starting" => "Starting the tunnel…".into(),
        "removing" => "Removing the tunnel…".into(),
        _ => "Pyo can't reach Cloudflare. Websites keep working.".into(),
    }
}

pub async fn tunnel_status(api: &Api, team: Option<&str>, cluster: Option<&str>) -> Result<()> {
    let (id, name) = cluster_of(api, team, cluster).await?;
    let v = api.get(&format!("/clusters/{id}/tunnel")).await?;
    println!("{name}: traffic {}", v["traffic"].as_str().unwrap_or("public"));
    let t = &v["tunnel"];
    if t.is_null() {
        println!("No Cloudflare Tunnel. Turn it on: pyo cloudflare tunnel on --cluster {name}");
        return Ok(());
    }
    println!("{}", tunnel_line(t));
    if let Some(e) = t["error"].as_str() {
        println!("{}", ui::yellow(e));
    }
    if t["refused"] == json!(true) {
        println!(
            "{}",
            ui::yellow("Run the command under Refresh this cluster in the cluster's settings.")
        );
    }
    for c in t["connectors"].as_array().into_iter().flatten() {
        let server = c["server"].as_str().unwrap_or("?");
        match c["reason"].as_str() {
            Some(r) if c["ready"] != json!(true) => println!("  {server}  {}", ui::red(r)),
            _ if c["ready"] == json!(true) => println!("  {server}  {}", ui::green("ready")),
            _ => println!("  {server}  {}", ui::dim("starting")),
        }
    }
    if let Some(d) = t["defaultDomain"].as_str() {
        println!("Default domain: *.{d}");
    }
    // Each domain through it, from the snapshot.
    let state = api.state().await?;
    for s in state.services().iter().filter(|s| s["clusterId"] == json!(id)) {
        for d in s["domains"].as_array().into_iter().flatten() {
            let Some(state) = d["tunnel"].as_str() else {
                continue;
            };
            let host = d["hostname"].as_str().unwrap_or("");
            let rec = &d["dnsRecord"];
            let what = match (state, rec["status"].as_str()) {
                (_, Some("conflict")) => ui::yellow("waiting for --replace"),
                (_, Some("reverting")) => ui::dim("switching back"),
                ("on", _) => ui::green("via Tunnel"),
                _ => ui::dim("switching"),
            };
            println!("  {host}  {what}");
        }
    }
    Ok(())
}

pub async fn tunnel_on(
    api: &Api,
    team: Option<&str>,
    cluster: Option<&str>,
    default_domain: Option<&str>,
    account: Option<&str>,
    ports_public: bool,
) -> Result<()> {
    let (id, name) = cluster_of(api, team, cluster).await?;
    let v = api
        .op(
            "setClusterTraffic",
            json!({ "clusterId": id, "mode": "tunnel", "defaultDomain": default_domain, "accountId": account, "portsPublic": ports_public }),
        )
        .await?;
    ui::ok(&format!("{name} takes websites through Cloudflare Tunnel"));
    let moving = v["moving"].as_u64().unwrap_or(0);
    if moving > 0 {
        println!("{moving} domains are moving. See them: pyo cloudflare tunnel status --cluster {name}");
    }
    Ok(())
}

pub async fn tunnel_off(api: &Api, team: Option<&str>, cluster: Option<&str>, to: &str) -> Result<()> {
    if !matches!(to, "public" | "tailscale") {
        bail!("--to is public or tailscale.");
    }
    let (id, name) = cluster_of(api, team, cluster).await?;
    api.op("setClusterTraffic", json!({ "clusterId": id, "mode": to }))
        .await?;
    ui::ok(&format!(
        "{name} takes traffic {}",
        if to == "public" { "on public ports" } else { "from Tailscale only" }
    ));
    println!("Domains keep using the tunnel until you move them back: pyo domain tunnel <hostname> --back");
    Ok(())
}

pub async fn tunnel_remove(
    api: &Api,
    team: Option<&str>,
    cluster: Option<&str>,
    restore: bool,
    yes: bool,
) -> Result<()> {
    let (id, name) = cluster_of(api, team, cluster).await?;
    if !yes {
        let a = ui::ask(&format!("Remove the Cloudflare Tunnel of {name}? [y/N] "))?;
        if !a.eq_ignore_ascii_case("y") {
            bail!("Not removed.");
        }
    }
    api.op(
        "removeClusterTunnel",
        json!({ "clusterId": id, "restoreRecords": restore }),
    )
    .await?;
    ui::ok("Removing the tunnel. Pyo deletes only the records it made.");
    Ok(())
}

pub async fn tunnel_rotate(api: &Api, team: Option<&str>, cluster: Option<&str>) -> Result<()> {
    let (id, name) = cluster_of(api, team, cluster).await?;
    api.op("rotateTunnelToken", json!({ "clusterId": id }))
        .await?;
    ui::ok(&format!("{name}'s tunnel gets a new token and certificate"));
    Ok(())
}

/// `pyo domain tunnel`: a domain through the tunnel, or back.
pub async fn domain_tunnel(
    api: &Api,
    service: Option<&str>,
    hostname: &str,
    on: bool,
    replace: bool,
    restore: bool,
) -> Result<()> {
    let t = crate::service::target(api, service).await?;
    let host = hostname.trim().trim_end_matches('.').to_lowercase();
    let d = t.service()["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["hostname"] == json!(host))
        .with_context(|| format!("{host} isn't one of {}'s domains.", t.state.path_of(t.service())))?;
    // A record already in the way, from an earlier try.
    let rec = &d["dnsRecord"];
    if on && !replace && rec["status"] == json!("conflict") {
        println!("{}", ui::yellow(&format!("Cloudflare has a record for {host}:")));
        for r in rec["existing"].as_array().into_iter().flatten() {
            println!(
                "  {}  {}  {}",
                r["type"].as_str().unwrap_or(""),
                r["name"].as_str().unwrap_or(""),
                r["content"].as_str().unwrap_or("")
            );
        }
        bail!("Run again with --replace to replace it. Pyo keeps a copy.");
    }
    let v = api
        .op(
            "setDomainTunnel",
            json!({ "serviceId": t.id, "domainId": d["id"], "on": on, "replace": replace, "restore": restore }),
        )
        .await?;
    match v["status"].as_str().unwrap_or("") {
        "active" if !on => ui::ok(&format!("{host} stays on the tunnel, under its default domain")),
        "active" => ui::ok(&format!("{host} goes through the tunnel")),
        "switching" => ui::ok(&format!("Switching {host} to the tunnel. If Cloudflare has a record Pyo didn't make, pyo cloudflare tunnel status shows it.")),
        "reverting" => ui::ok(&format!("Switching {host} back")),
        _ => ui::ok(&format!("{host} doesn't go through the tunnel")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_by_longest_suffix() {
        let s = json!({ "cloudflare": [{ "teamId": "t1", "zones": [
            { "name": "example.com", "canEdit": true },
            { "name": "eu.example.com", "canEdit": true },
        ] }] });
        let z = |h: &str| zone_for(&s, "t1", h).map(|z| z["name"].as_str().unwrap());
        assert_eq!(z("a.example.com"), Some("example.com"));
        assert_eq!(z("a.eu.example.com"), Some("eu.example.com"));
        assert_eq!(z("example.com"), Some("example.com"));
        assert_eq!(z("notexample.com"), None);
        assert_eq!(zone_for(&s, "t2", "a.example.com"), None);
    }

    #[test]
    fn the_tunnel_column_says_what_a_zone_can_do() {
        let plain = |t: Value| {
            tunnel_column(&t)
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
        };
        assert!(plain(json!({ "ready": true })).contains("ready"));
        assert!(plain(json!({ "ready": false, "missing": ["SSL and Certificates:Edit"] })).contains("needs SSL and Certificates:Edit"));
        assert!(plain(json!({ "ready": false, "missing": ["a", "b"] })).contains("DNS only"));
        // Stored before the tunnel: DNS only.
        assert!(plain(Value::Null).contains("DNS only"));
    }

    #[test]
    fn tunnel_lines() {
        let t = json!({ "health": "connected", "colos": ["AMS", "FRA"], "connectors": [{ "ready": true }, { "ready": true }] });
        assert_eq!(tunnel_line(&t), "Tunnel connected · 2 connectors · AMS, FRA");
        let t = json!({ "health": "degraded", "connectors": [{ "ready": true }, { "ready": false }] });
        assert_eq!(tunnel_line(&t), "Tunnel degraded · 1 of 2 connectors");
        assert_eq!(tunnel_line(&json!({ "health": "down" })), "Tunnel down. Its websites don't answer.");
        assert_eq!(tunnel_line(&json!({})), "Pyo can't reach Cloudflare. Websites keep working.");
    }

    #[test]
    fn tokens_come_from_a_file_or_the_environment() {
        assert_eq!(read_token(None, Some(" abc \n".into())).unwrap(), "abc");
        assert!(read_token(None, None).is_err());
        assert!(read_token(Some("/nonexistent/token"), None).is_err());
    }
}
