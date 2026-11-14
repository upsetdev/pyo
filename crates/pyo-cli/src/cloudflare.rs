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
            "Give the token in a file (--token-file, - for stdin) or in CLOUDFLARE_API_TOKEN. It needs Zone:Read and DNS:Edit."
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
    println!("{}", ui::dim(&format!("{:<w$}  DNS", "ZONE")));
    for z in zones {
        let name = z["name"].as_str().unwrap_or("");
        let dns = if z["canEdit"] == json!(true) {
            ui::green("can edit")
        } else {
            ui::red(&format!(
                "needs {}",
                z["missing"].as_str().unwrap_or("DNS:Edit")
            ))
        };
        println!("{name:<w$}  {dns}");
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
    fn tokens_come_from_a_file_or_the_environment() {
        assert_eq!(read_token(None, Some(" abc \n".into())).unwrap(), "abc");
        assert!(read_token(None, None).is_err());
        assert!(read_token(Some("/nonexistent/token"), None).is_err());
    }
}
