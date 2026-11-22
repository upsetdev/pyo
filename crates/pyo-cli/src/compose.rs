//! `pyo deploy -f compose.yml`: a Docker Compose file as a project. The
//! first time, Pyo shows what it makes of the file (and what it leaves out),
//! then creates and deploys it once you say yes. After that, the same
//! command syncs the project with the file: Pyo shows what changes and
//! applies only that. `--dry-run` only shows it, and `--prune` also deletes
//! the services no longer in the file.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{api::Api, ui};

pub struct Opts<'a> {
    pub team: Option<&'a str>,
    pub cluster: Option<&'a str>,
    /// The project to sync into (name, namespace or id).
    pub project: Option<&'a str>,
    pub dry_run: bool,
    pub prune: bool,
    pub yes: bool,
}

pub async fn deploy(api: &Api, file: &Path, o: Opts<'_>) -> Result<()> {
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
    let file_name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let (team_id, cluster_id) = team_and_cluster(api, o.team, o.cluster).await?;
    let project_id = match o.project {
        Some(p) => Some(project_in(api, &team_id, p).await?),
        None => None,
    };

    let plan = api
        .post(
            "/api/v1/compose/sync-plan",
            &json!({ "teamId": team_id, "projectId": project_id, "file": text, "env": env, "clusterId": cluster_id, "name": name, "fileName": file_name }),
        )
        .await?;
    if plan["project"].is_null() {
        return import(api, &plan["file"], &team_id, &cluster_id, &o).await;
    }

    print_sync(&plan, o.prune);
    let services = plan["services"].as_array().cloned().unwrap_or_default();
    let changing = services
        .iter()
        .filter(|s| matches!(s["status"].as_str(), Some("added" | "changed")))
        .count();
    let deleting = services
        .iter()
        .filter(|s| s["status"] == "notInFile" && s["fromFile"] == true && o.prune)
        .count();
    if changing == 0 && deleting == 0 {
        println!("Nothing to change.");
        return Ok(());
    }
    if o.dry_run {
        println!("Dry run: nothing was changed.");
        return Ok(());
    }
    let project = plan["project"]["name"].as_str().unwrap_or("the project");
    if !o.yes {
        let mut q = format!("Apply this to {project}?");
        if deleting > 0 {
            q = format!(
                "Apply this to {project} and delete {deleting} service{}?",
                if deleting == 1 { "" } else { "s" }
            );
        }
        let a = ui::ask(&format!("{q} [y/N] "))?;
        if !matches!(a.to_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was changed.");
            return Ok(());
        }
    }
    let mut body = json!({ "projectId": plan["project"]["id"], "plan": plan["file"], "prune": o.prune, "confirm": o.yes });
    // New services run where the project's do, unless --cluster says.
    if o.cluster.is_some() {
        body["clusterId"] = json!(cluster_id);
    }
    let r = api.op("syncCompose", body).await?;
    let mut deploying = false;
    for s in r["services"].as_array().into_iter().flatten() {
        let status = s["status"].as_str().unwrap_or("");
        let dep = s["deploymentId"].as_str().unwrap_or("");
        deploying |= !dep.is_empty();
        println!(
            "{} {} {}",
            ui::green("✓"),
            s["name"].as_str().unwrap_or(""),
            ui::dim(if dep.is_empty() { status } else { dep })
        );
        if let Some(w) = s["warning"].as_str() {
            println!("  {} {w}", ui::yellow("!"));
        }
    }
    if deploying {
        ui::ok(&format!(
            "Deploying. Follow it in the dashboard: {}",
            api.base
        ));
    } else {
        ui::ok("Done.");
    }
    Ok(())
}

/// The first time: a new project from the file.
async fn import(
    api: &Api,
    plan: &Value,
    team_id: &str,
    cluster_id: &str,
    o: &Opts<'_>,
) -> Result<()> {
    print_plan(plan);
    if o.dry_run {
        println!("Dry run: nothing was created.");
        return Ok(());
    }
    let services = plan["services"].as_array().map(Vec::len).unwrap_or(0);
    if !o.yes {
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

/// A project of the team by id, namespace or name.
async fn project_in(api: &Api, team_id: &str, p: &str) -> Result<String> {
    let state = api.state().await?;
    state.0["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|x| x["teamId"] == json!(team_id))
        .find(|x| x["id"] == json!(p) || x["namespace"] == json!(p) || x["name"] == json!(p))
        .and_then(|x| x["id"].as_str())
        .map(str::to_owned)
        .with_context(|| format!("No project {p} in this team"))
}

/// The team (slug or id, else the first) and its cluster (name or id, else
/// the team's default) an import goes to.
pub async fn team_and_cluster(
    api: &Api,
    team: Option<&str>,
    cluster: Option<&str>,
) -> Result<(String, String)> {
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
    Ok((team_id, cluster_id))
}

/// A sync plan, service by service.
pub fn sync_lines(plan: &Value, prune: bool) -> Vec<String> {
    let mut out = vec![];
    let file = plan["file"]["project"]["file"].as_str().unwrap_or("");
    out.push(format!(
        "{} {}",
        ui::bold(plan["project"]["name"].as_str().unwrap_or("")),
        ui::dim(&format!(
            "(synced with {})",
            if file.is_empty() { "its Compose file" } else { file }
        ))
    ));
    let mut manual = vec![];
    for s in plan["services"].as_array().into_iter().flatten() {
        let name = s["name"].as_str().unwrap_or("");
        let from_file = s["fromFile"] == true;
        let (mark, words) = match s["status"].as_str().unwrap_or("") {
            "added" => (ui::green("+"), "added".to_owned()),
            "changed" => (ui::yellow("~"), "changed".to_owned()),
            "notInFile" if from_file && prune => {
                (ui::red("-"), "not in the file, so it's deleted".to_owned())
            }
            "notInFile" if from_file => (
                ui::red("-"),
                "not in the file. --prune deletes it.".to_owned(),
            ),
            "notInFile" => (ui::dim("-"), "not in the file, kept".to_owned()),
            _ => (ui::dim("="), "unchanged".to_owned()),
        };
        out.push(format!("  {mark} {} {}", ui::bold(name), ui::dim(&words)));
        for c in s["changes"].as_array().into_iter().flatten() {
            let field = c["field"].as_str().unwrap_or("");
            let line = match (c["from"].as_str(), c["to"].as_str()) {
                (Some(a), Some(b)) => format!("{field}: {a} → {b}"),
                (None, Some(b)) => format!("{field}: {b}"),
                _ => field.to_owned(),
            };
            out.push(format!("      {line}"));
        }
        for n in s["notes"].as_array().into_iter().flatten() {
            out.push(format!(
                "      {}",
                ui::dim(&format!(
                    "{}: {}",
                    n["field"].as_str().unwrap_or(""),
                    n["message"].as_str().unwrap_or("")
                ))
            ));
        }
        for m in s["manual"].as_array().into_iter().flatten() {
            manual.push(format!(
                "  {} {name}, {}: {}",
                ui::yellow("!"),
                m["field"].as_str().unwrap_or(""),
                m["message"].as_str().unwrap_or("")
            ));
        }
    }
    if !manual.is_empty() {
        out.push(ui::yellow("Pyo leaves these as they are. Change them by hand:"));
        out.extend(manual);
    }
    out.extend(warning_lines(&plan["warnings"]));
    out
}

fn print_sync(plan: &Value, prune: bool) {
    for l in sync_lines(plan, prune) {
        println!("{l}");
    }
}

fn warning_lines(warnings: &Value) -> Vec<String> {
    let warnings = warnings.as_array().cloned().unwrap_or_default();
    if warnings.is_empty() {
        return vec![];
    }
    let mut out = vec![ui::yellow("Not carried over, or changed:")];
    for w in warnings {
        let who = w["service"]
            .as_str()
            .map(|s| format!("{s}: "))
            .unwrap_or_default();
        out.push(format!(
            "  {} {who}{}",
            ui::yellow("!"),
            w["message"].as_str().unwrap_or("")
        ));
    }
    out
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
        let domains: Vec<&str> = s["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .skip(1)
            .collect();
        if !domains.is_empty() {
            println!("    {} {}", ui::dim("domains"), domains.join(", "));
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
        let secrets: Vec<&str> = s["secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !secrets.is_empty() {
            println!(
                "    {} {} {}",
                ui::dim("secrets"),
                secrets.join(", "),
                ui::dim("(set their values in Pyo)")
            );
        }
    }
    for l in warning_lines(&plan["warnings"]) {
        println!("{l}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: Vec<String>) -> String {
        let text = lines.join("\n");
        // No colors in the comparison.
        let mut out = String::new();
        let mut skip = false;
        for c in text.chars() {
            match (skip, c) {
                (_, '\x1b') => skip = true,
                (true, 'm') => skip = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    #[test]
    fn a_sync_plan_reads_service_by_service() {
        let plan = json!({
            "project": { "id": "prj_a", "name": "cloud", "namespace": "cloud" },
            "file": { "project": { "file": "compose.yaml" } },
            "services": [
                { "name": "cloud-api", "status": "changed", "fromFile": true, "deploy": true,
                  "changes": [
                    { "field": "image", "from": "ghcr.io/acme/cloud-api:1.2", "to": "ghcr.io/acme/cloud-api:1.3" },
                    { "field": "env LOG_LEVEL" },
                    { "field": "domain api.example.com", "to": "added" }],
                  "notes": [{ "field": "env DATABASE_URL", "message": "It's a secret in Pyo, so its value stays." }],
                  "manual": [] },
                { "name": "worker", "status": "added", "changes": [], "notes": [], "manual": [] },
                { "name": "db", "status": "unchanged", "fromFile": true, "changes": [], "notes": [],
                  "manual": [{ "field": "volume db-data", "message": "It's mounted at /var/lib/postgresql/data in Pyo and /data in the file. Detach it and attach it at /data to move it." }] },
                { "name": "old", "status": "notInFile", "fromFile": true, "changes": [], "notes": [], "manual": [] },
                { "name": "hand", "status": "notInFile", "fromFile": false, "changes": [], "notes": [], "manual": [] }
            ],
            "warnings": []
        });
        let text = plain(sync_lines(&plan, false));
        assert_eq!(
            text,
            "cloud (synced with compose.yaml)
  ~ cloud-api changed
      image: ghcr.io/acme/cloud-api:1.2 → ghcr.io/acme/cloud-api:1.3
      env LOG_LEVEL
      domain api.example.com: added
      env DATABASE_URL: It's a secret in Pyo, so its value stays.
  + worker added
  = db unchanged
  - old not in the file. --prune deletes it.
  - hand not in the file, kept
Pyo leaves these as they are. Change them by hand:
  ! db, volume db-data: It's mounted at /var/lib/postgresql/data in Pyo and /data in the file. Detach it and attach it at /data to move it."
        );
        assert!(plain(sync_lines(&plan, true)).contains("- old not in the file, so it's deleted"));
    }
}
