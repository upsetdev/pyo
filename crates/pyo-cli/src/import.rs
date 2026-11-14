//! `pyo import coolify|dokploy|railway|vercel`: bring a team's services
//! over from another platform. Pyo reads it with the token given (and never keeps it), shows
//! what each thing becomes and what doesn't carry over, then imports
//! what's importable once you say yes.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::{api::Api, compose::team_and_cluster, ui};

pub struct Args<'a> {
    pub platform: &'a str,
    pub from: &'a str,
    pub token: &'a str,
    /// Vercel's team.
    pub source_team: Option<&'a str>,
    pub team: Option<&'a str>,
    pub cluster: Option<&'a str>,
    pub deploy: bool,
    pub yes: bool,
}

pub async fn run(api: &Api, a: Args<'_>) -> Result<()> {
    let (team_id, cluster_id) = team_and_cluster(api, a.team, a.cluster).await?;
    if a.source_team.is_some() && a.platform != "vercel" {
        bail!("--source-team is for Vercel only.");
    }
    let mut source =
        json!({ "teamId": team_id, "platform": a.platform, "url": a.from, "token": a.token });
    if let Some(t) = a.source_team {
        source["team"] = json!(t);
    }
    let plan = api
        .post("/api/v1/platform-imports/preview", &source)
        .await?;
    let keys = print_plan(&plan);
    if keys.is_empty() {
        println!("Nothing new to import.");
        return Ok(());
    }
    if !a.yes {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            bail!("No terminal to ask on: pass -y to confirm.");
        }
        let n = keys.len();
        let answer = ui::ask(&format!(
            "Import {n} thing{}{}? [y/N] ",
            if n == 1 { "" } else { "s" },
            if a.deploy {
                " and deploy the services"
            } else {
                ""
            }
        ))?;
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was imported.");
            return Ok(());
        }
    }
    let mut body = source;
    body["clusterId"] = json!(cluster_id);
    body["items"] = json!(keys);
    body["deploy"] = json!(a.deploy);
    // It runs on the server: stopping here doesn't stop it. Follow it.
    let run = api.post("/api/v1/platform-imports", &body).await?;
    let id = run["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        bail!("Pyo didn't say which import it started.");
    }
    println!(
        "{}",
        ui::dim("Importing. It goes on in Pyo if you stop this.")
    );
    let mut shown = 0;
    let run = loop {
        let run = api.get(&format!("/api/v1/platform-imports/{id}")).await?;
        let results = run["results"].as_array().cloned().unwrap_or_default();
        for r in results.iter().skip(shown) {
            print_result(r);
        }
        shown = shown.max(results.len());
        if !matches!(run["status"].as_str(), Some("reading" | "importing")) {
            break run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    };
    match run["status"].as_str() {
        Some("failed") => bail!(
            "The import stopped: {}",
            run["error"].as_str().unwrap_or("no reason given")
        ),
        Some("interrupted") => {
            bail!("Pyo restarted during the import. Run it again: what's there already is skipped.")
        }
        _ => {}
    }
    ui::ok(&format!("Done. See it in the dashboard: {}", api.base));
    Ok(())
}

fn print_result(r: &Value) {
    let state = r["state"].as_str().unwrap_or("");
    let mark = match state {
        "created" | "importing" => ui::green("✓"),
        "failed" => ui::red("✗"),
        _ => ui::dim("·"),
    };
    let label = match state {
        "created" => "created",
        "importing" => "copying its data",
        "exists" => "imported before",
        "skipped" => "not imported",
        _ => "failed",
    };
    let msg = r["message"].as_str().unwrap_or("");
    println!(
        "{mark} {} {}{}",
        r["name"].as_str().unwrap_or(""),
        ui::dim(label),
        if msg.is_empty() {
            String::new()
        } else {
            format!(": {msg}")
        }
    );
}

/// Prints the plan; returns the keys to import (all that can be).
fn print_plan(plan: &Value) -> Vec<String> {
    let mut keys = vec![];
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let notes = |w: &Value| {
        for n in list(w) {
            println!("      {} {}", ui::yellow("!"), n.as_str().unwrap_or(""));
        }
    };
    for w in list(&plan["warnings"]) {
        println!("{} {}", ui::yellow("!"), w.as_str().unwrap_or(""));
    }
    for p in list(&plan["projects"]) {
        println!("{}", ui::bold(p["name"].as_str().unwrap_or("")));
        for s in list(&p["services"]) {
            let src = &s["service"]["source"];
            let from = match src["type"].as_str() {
                Some("git") => format!(
                    "{} {}",
                    src["repoUrl"].as_str().unwrap_or(""),
                    src["branch"].as_str().unwrap_or("")
                ),
                Some("image") => format!(
                    "{}:{}",
                    src["image"].as_str().unwrap_or(""),
                    src["reference"].as_str().unwrap_or("")
                ),
                _ => String::new(),
            };
            let name = s["service"]["name"].as_str().unwrap_or("");
            item(&s, name, &from, &mut keys);
            for d in list(&s["domains"]) {
                println!(
                    "      {} {}",
                    ui::dim("domain"),
                    d["hostname"].as_str().unwrap_or("")
                );
            }
            notes(&s["warnings"]);
        }
        for st in list(&p["stacks"]) {
            let n = st["plan"]["services"].as_array().map(Vec::len).unwrap_or(0);
            let name = st["plan"]["project"]["name"].as_str().unwrap_or("");
            item(
                &st,
                name,
                &format!("Compose, {n} services, a project of its own"),
                &mut keys,
            );
            notes(&st["warnings"]);
        }
        for d in list(&p["databases"]) {
            let what = match d["action"].as_str() {
                Some("import") => "data copied",
                Some("create") => "made empty",
                _ => "not imported",
            };
            let from = format!("{} database, {what}", d["engine"].as_str().unwrap_or(""));
            item(&d, d["name"].as_str().unwrap_or(""), &from, &mut keys);
            notes(&d["warnings"]);
        }
    }
    keys
}

fn item(v: &Value, name: &str, from: &str, keys: &mut Vec<String>) {
    let before = v["existingId"].is_string() || v["existingProjectId"].is_string();
    let can = v["importable"]
        .as_bool()
        .unwrap_or(v["action"] != json!("manual"));
    let tag = if before {
        ui::dim("imported before")
    } else if !can {
        ui::yellow("not imported")
    } else {
        keys.push(v["key"].as_str().unwrap_or("").to_owned());
        String::new()
    };
    println!("  {} {} {tag}", name, ui::dim(from));
}
