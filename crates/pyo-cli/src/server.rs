//! `pyo hetzner` and `pyo server`: connect a team to Hetzner Cloud, then
//! make servers there that join a cluster over the tailnet, follow them to
//! ready and delete them. Every create and delete shows its monthly price
//! and asks first (or -y).

use std::io::{IsTerminal, Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    api::{Api, NeedsConfirm, State, new_id},
    ui,
};

/// A team by slug or id, else the first.
fn team<'a>(state: &'a State, team: Option<&str>) -> Result<&'a Value> {
    let teams = state.teams();
    match team {
        Some(t) => teams
            .iter()
            .find(|x| x["slug"] == json!(t) || x["id"] == json!(t))
            .with_context(|| format!("No team {t} among yours")),
        None => teams.first().context("You aren't in any team"),
    }
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap_or("").to_owned()
}

fn list<'a>(state: &'a State, key: &str) -> &'a [Value] {
    state.0[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// The token: PYO_HETZNER_TOKEN, piped in, or typed without echo.
fn read_token(given: Option<String>) -> Result<String> {
    if let Some(t) = given.filter(|t| !t.trim().is_empty()) {
        return Ok(t.trim().to_owned());
    }
    let mut s = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(s.trim().to_owned());
    }
    print!("Hetzner API token (Read & Write): ");
    std::io::stdout().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut byte = [0u8; 1];
    let read = loop {
        match std::io::stdin().read(&mut byte) {
            Ok(1) if byte[0] == b'\r' || byte[0] == b'\n' => break Ok(()),
            Ok(1) if byte[0] == 3 => break Err(anyhow::anyhow!("Cancelled.")),
            Ok(1) if byte[0] == 127 || byte[0] == 8 => {
                s.pop();
            }
            Ok(1) => s.push(byte[0] as char),
            Ok(_) => break Ok(()),
            Err(e) => break Err(e.into()),
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    println!();
    read?;
    Ok(s.trim().to_owned())
}

pub async fn connect(api: &Api, team_ref: Option<&str>, token: Option<String>) -> Result<()> {
    let state = api.state().await?;
    let t = team(&state, team_ref)?;
    let token = read_token(token)?;
    let r = api
        .op("saveHetzner", json!({ "teamId": id_of(t), "token": token }))
        .await?;
    let n = r["locations"].as_array().map_or(0, Vec::len);
    ui::ok(&format!(
        "Connected Hetzner to {}. The token sees {n} locations.",
        t["name"].as_str().unwrap_or("")
    ));
    Ok(())
}

pub async fn test(api: &Api, team_ref: Option<&str>) -> Result<()> {
    let state = api.state().await?;
    let t = team(&state, team_ref)?;
    let r = api.op("testHetzner", json!({ "teamId": id_of(t) })).await?;
    for l in r["locations"].as_array().into_iter().flatten() {
        println!(
            "{}  {}",
            ui::bold(l["name"].as_str().unwrap_or("")),
            ui::dim(&format!(
                "{}, {}",
                l["city"].as_str().unwrap_or(""),
                l["country"].as_str().unwrap_or("")
            ))
        );
    }
    ui::ok("Hetzner answers.");
    Ok(())
}

pub async fn disconnect(api: &Api, team_ref: Option<&str>, yes: bool) -> Result<()> {
    let state = api.state().await?;
    let t = team(&state, team_ref)?;
    if !yes && !yes_to("Forget the Hetzner token? Servers Pyo made there keep running. [y/N] ")? {
        bail!("Kept.");
    }
    api.op("removeHetzner", json!({ "teamId": id_of(t) }))
        .await?;
    ui::ok("Hetzner disconnected.");
    Ok(())
}

fn yes_to(q: &str) -> Result<bool> {
    Ok(matches!(
        ui::ask(q)?.trim().to_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn money(currency: &str, n: f64) -> String {
    match currency {
        "EUR" => format!("€{n:.2}"),
        "USD" => format!("${n:.2}"),
        c => format!("{n:.2} {c}"),
    }
}

pub async fn types(api: &Api, team_ref: Option<&str>, location: Option<&str>) -> Result<()> {
    let state = api.state().await?;
    let t = team(&state, team_ref)?;
    let o = api
        .get(&format!("/api/v1/cloud/options?team={}", id_of(t)))
        .await?;
    let currency = o["currency"].as_str().unwrap_or("EUR");
    let locations: Vec<&Value> = o["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|l| location.is_none_or(|w| l["name"] == json!(w)))
        .collect();
    if locations.is_empty() {
        bail!("Hetzner has no location {}.", location.unwrap_or(""));
    }
    for l in locations {
        let loc = l["name"].as_str().unwrap_or("");
        println!(
            "{} {}",
            ui::bold(loc),
            ui::dim(&format!(
                "{}, {}",
                l["city"].as_str().unwrap_or(""),
                l["country"].as_str().unwrap_or("")
            ))
        );
        for st in o["serverTypes"].as_array().into_iter().flatten() {
            let Some(p) = st["prices"][loc].as_f64() else {
                continue;
            };
            let available = st["available"]
                .as_array()
                .is_some_and(|a| a.contains(&json!(loc)));
            let note = if st["supported"] != true {
                " (Arm, not for Pyo)"
            } else if !available {
                " (unavailable)"
            } else {
                ""
            };
            let line = format!(
                "  {:<8} {:>2} vCPU {:>4} GB {:>5} GB disk  {:<4} {:>8}/mo{note}",
                st["name"].as_str().unwrap_or(""),
                st["cores"],
                st["memoryGb"],
                st["diskGb"],
                st["architecture"].as_str().unwrap_or(""),
                money(currency, p),
            );
            println!(
                "{}",
                if note.is_empty() {
                    line
                } else {
                    ui::dim(&line)
                }
            );
        }
    }
    println!("{}", ui::dim("Monthly, before VAT, with the IPv4 address."));
    Ok(())
}

pub struct New<'a> {
    pub team: Option<&'a str>,
    pub cluster: Option<&'a str>,
    pub new_cluster: Option<&'a str>,
    pub name: Option<&'a str>,
    pub location: &'a str,
    pub server_type: &'a str,
    pub builder: bool,
    pub ssh_key: Option<&'a std::path::Path>,
    pub tags: Vec<String>,
    pub yes: bool,
    pub detach: bool,
}

/// Sends an operation, showing its price and asking first.
async fn priced(api: &Api, op: &str, args: Value, yes: bool) -> Result<Value> {
    match api.op(op, args.clone()).await {
        Err(e) if e.downcast_ref::<NeedsConfirm>().is_some() => {
            println!("{e}");
            if !yes && !yes_to("Go ahead? [y/N] ")? {
                bail!("Nothing was changed.");
            }
            let mut args = args;
            args["confirm"] = json!(true);
            api.op(op, args).await
        }
        r => r,
    }
}

pub async fn create(api: &Api, n: New<'_>) -> Result<()> {
    let state = api.state().await?;
    let ssh_key = match n.ssh_key {
        Some(p) => Some(
            std::fs::read_to_string(p)
                .with_context(|| format!("Can't read {}", p.display()))?
                .trim()
                .to_owned(),
        ),
        None => None,
    };
    let id = new_id("cs");
    let (op, args) = if let Some(cluster_name) = n.new_cluster {
        let t = team(&state, n.team)?;
        (
            "createCloudCluster",
            json!({ "id": new_id("cl"), "teamId": id_of(t), "name": cluster_name, "serverId": id,
                    "location": n.location, "serverType": n.server_type, "sshKey": ssh_key, "tags": n.tags }),
        )
    } else {
        let (_, cluster_id) = crate::compose::team_and_cluster(api, n.team, n.cluster).await?;
        let taken: Vec<&str> = list(&state, "servers")
            .iter()
            .filter(|s| s["clusterId"] == json!(cluster_id))
            .filter_map(|s| s["name"].as_str())
            .chain(
                list(&state, "cloudServers")
                    .iter()
                    .filter(|s| s["clusterId"] == json!(cluster_id))
                    .filter_map(|s| s["name"].as_str()),
            )
            .collect();
        let cluster_name = state
            .cluster(&cluster_id)
            .and_then(|c| c["name"].as_str())
            .unwrap_or("server")
            .to_owned();
        let name = match n.name {
            Some(x) => x.to_owned(),
            None => (1..)
                .map(|i| format!("{cluster_name}-{i}"))
                .find(|x| !taken.contains(&x.as_str()))
                .unwrap_or_default(),
        };
        (
            "createCloudServer",
            json!({ "id": id, "clusterId": cluster_id, "name": name, "location": n.location,
                    "serverType": n.server_type, "builder": n.builder, "sshKey": ssh_key, "tags": n.tags }),
        )
    };
    let r = priced(api, op, args, n.yes).await?;
    let name = r["name"].as_str().unwrap_or("").to_owned();
    ui::ok(&format!("Creating {name} at Hetzner."));
    if n.detach {
        return Ok(());
    }
    follow(api, &id).await
}

/// Follows a new server to ready, saying each step.
async fn follow(api: &Api, id: &str) -> Result<()> {
    let mut last = String::new();
    let mut sign_shown = false;
    loop {
        let state = api.state().await?;
        let Some(s) = list(&state, "cloudServers")
            .iter()
            .find(|s| s["id"] == json!(id))
        else {
            bail!("The server is gone.");
        };
        let st = s["state"].as_str().unwrap_or("").to_owned();
        if st != last {
            let label = match st.as_str() {
                "creating" => "Creating",
                "booting" => "Booting",
                "joining" => "Joining",
                "ready" => "Ready",
                "failed" => "Failed",
                other => other,
            };
            println!("{} {label}", ui::dim("·"));
            last = st.clone();
        }
        if let Some(cmd) = s["signCommand"].as_str()
            && !sign_shown
        {
            println!("Your tailnet has Tailnet Lock on. On a signing device, run:\n  {cmd}");
            sign_shown = true;
        }
        match st.as_str() {
            "ready" => {
                ui::ok(&format!("{} is ready.", s["name"].as_str().unwrap_or("")));
                return Ok(());
            }
            "failed" => bail!(
                "{} Delete it with: pyo server delete {}",
                s["error"].as_str().unwrap_or("It didn't join."),
                s["name"].as_str().unwrap_or("")
            ),
            _ => tokio::time::sleep(Duration::from_secs(5)).await,
        }
    }
}

pub async fn list_servers(api: &Api) -> Result<()> {
    let state = api.state().await?;
    let all = list(&state, "cloudServers");
    if all.is_empty() {
        println!("No servers made at Hetzner.");
        return Ok(());
    }
    for s in all {
        let cluster = s["clusterId"]
            .as_str()
            .and_then(|c| state.cluster(c))
            .and_then(|c| c["name"].as_str())
            .unwrap_or("(removed)");
        println!(
            "{}  {}  {} in {}  {}/mo  {}",
            ui::bold(s["name"].as_str().unwrap_or("")),
            cluster,
            s["serverType"].as_str().unwrap_or(""),
            s["location"].as_str().unwrap_or(""),
            money(
                s["currency"].as_str().unwrap_or("EUR"),
                s["monthly"].as_f64().unwrap_or(0.0)
            ),
            s["state"].as_str().unwrap_or("")
        );
        if let Some(e) = s["error"].as_str() {
            println!("  {} {e}", ui::red("error:"));
        }
        if let Some(c) = s["signCommand"].as_str() {
            println!("  {} {c}", ui::yellow("sign:"));
        }
    }
    Ok(())
}

pub async fn delete(api: &Api, name: &str, cluster: Option<&str>, yes: bool) -> Result<()> {
    let state = api.state().await?;
    let hits: Vec<&Value> = list(&state, "cloudServers")
        .iter()
        .filter(|s| s["id"] == json!(name) || s["name"] == json!(name))
        .filter(|s| {
            cluster.is_none_or(|c| {
                s["clusterId"] == json!(c)
                    || s["clusterId"]
                        .as_str()
                        .and_then(|id| state.cluster(id))
                        .is_some_and(|x| x["name"] == json!(c))
            })
        })
        .collect();
    let s = match hits.len() {
        0 => bail!("No server {name} made at Hetzner. See them: pyo server list"),
        1 => hits[0],
        _ => bail!("More than one server is called {name}. Pass --cluster."),
    };
    priced(api, "deleteCloudServer", json!({ "id": s["id"] }), yes).await?;
    ui::ok(&format!(
        "Deleting {name}. Its services move off it first, then it's deleted at Hetzner."
    ));
    Ok(())
}
