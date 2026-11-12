//! `pyo deploy -f compose.yml`: import a Docker Compose file as a project.
//! Pyo shows what it makes of the file (and what it leaves out), then
//! creates and deploys it once you say yes.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{api::Api, ui};

pub async fn deploy(
    api: &Api,
    file: &Path,
    team: Option<&str>,
    cluster: Option<&str>,
    yes: bool,
) -> Result<()> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("Couldn't read {}", file.display()))?;
    let dir = file
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let env = std::fs::read_to_string(dir.join(".env")).ok();
    let name = std::fs::canonicalize(dir)
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let state = api.state().await?;
    let teams = state.teams();
    let team = match team {
        Some(t) => teams
            .iter()
            .find(|x| x["slug"] == json!(t) || x["id"] == json!(t))
            .with_context(|| format!("No team {t} among yours"))?,
        None => teams.first().context("You aren't in any team")?,
    };
    let team_id = team["id"].as_str().unwrap_or("").to_owned();
    let clusters: Vec<&Value> = state.0["clusters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["teamId"] == json!(team_id))
        .collect();
    let cluster_id = match cluster {
        Some(c) => clusters
            .iter()
            .find(|x| x["name"] == json!(c) || x["id"] == json!(c))
            .and_then(|x| x["id"].as_str())
            .with_context(|| format!("No cluster {c} in this team"))?
            .to_owned(),
        None => team["defaultClusterId"]
            .as_str()
            .or_else(|| clusters.first().and_then(|c| c["id"].as_str()))
            .context("This team has no cluster to deploy to. Add a server first.")?
            .to_owned(),
    };

    let plan = api
        .post("/api/v1/compose/preview", &json!({ "teamId": team_id, "file": text, "env": env, "clusterId": cluster_id, "name": name }))
        .await?;
    print_plan(&plan);

    let services = plan["services"].as_array().map(Vec::len).unwrap_or(0);
    if !yes {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            bail!("No terminal to ask on: pass -y to confirm.");
        }
        let a = ui::ask(&format!(
            "Create {} and deploy its {services} service{}? [y/N] ",
            plan["project"]["name"].as_str().unwrap_or("the project"),
            if services == 1 { "" } else { "s" }
        ))?;
        if !matches!(a.to_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was created.");
            return Ok(());
        }
    }
    let r = api
        .op(
            "importCompose",
            json!({ "teamId": team_id, "clusterId": cluster_id, "plan": plan }),
        )
        .await?;
    for s in r["services"].as_array().into_iter().flatten() {
        println!(
            "{} {} {}",
            ui::green("✓"),
            s["name"].as_str().unwrap_or(""),
            ui::dim(s["deploymentId"].as_str().unwrap_or(""))
        );
    }
    ui::ok(&format!(
        "Deploying. Follow it in the dashboard: {}",
        api.base
    ));
    Ok(())
}

fn print_plan(plan: &Value) {
    println!(
        "{} {}",
        ui::bold(plan["project"]["name"].as_str().unwrap_or("")),
        ui::dim("(new project)")
    );
    for s in plan["services"].as_array().into_iter().flatten() {
        let src = &s["source"];
        let from = if src["type"] == "git" {
            format!(
                "built from {}",
                if src["buildContext"].as_str().unwrap_or("").is_empty() {
                    "the repository"
                } else {
                    src["buildContext"].as_str().unwrap_or("")
                }
            )
        } else {
            format!(
                "{}:{}",
                src["image"].as_str().unwrap_or(""),
                src["reference"].as_str().unwrap_or("")
            )
        };
        println!(
            "  {} {}",
            ui::bold(s["name"].as_str().unwrap_or("")),
            ui::dim(&from)
        );
        for p in s["ports"].as_array().into_iter().flatten() {
            let port = p["containerPort"].as_u64().unwrap_or(0);
            let line = match (
                p["public"].as_bool().unwrap_or(false),
                p["protocol"].as_str().unwrap_or(""),
            ) {
                (true, "HTTP") => format!(
                    "{port} public at {}",
                    p["domain"].as_str().unwrap_or("a domain you add")
                ),
                (true, proto) => format!(
                    "{port}/{} public on {}",
                    proto.to_lowercase(),
                    p["publicPort"].as_u64().unwrap_or(port)
                ),
                (false, _) => format!("{port} inside the project"),
            };
            println!("    {} {line}", ui::dim("port"));
        }
        for v in s["volumes"].as_array().into_iter().flatten() {
            println!(
                "    {} {} at {} ({} GiB{})",
                ui::dim("volume"),
                v["name"].as_str().unwrap_or(""),
                v["mountPath"].as_str().unwrap_or(""),
                v["sizeGi"],
                if v["fromBind"] == json!(true) {
                    ", starts empty"
                } else {
                    ""
                }
            );
        }
        let env = s["env"].as_array().map(Vec::len).unwrap_or(0);
        if env > 0 {
            println!("    {} {env}", ui::dim("variables"));
        }
    }
    let warnings = plan["warnings"].as_array().cloned().unwrap_or_default();
    if !warnings.is_empty() {
        println!("{}", ui::yellow("Not carried over, or changed:"));
        for w in warnings {
            let who = w["service"]
                .as_str()
                .map(|s| format!("{s}: "))
                .unwrap_or_default();
            println!(
                "  {} {who}{}",
                ui::yellow("!"),
                w["message"].as_str().unwrap_or("")
            );
        }
    }
}
