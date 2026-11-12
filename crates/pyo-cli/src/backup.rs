//! `pyo backup now`: back up a volume or database right away, as the
//! dashboard's "Back up now" does, to the team's backup destination.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{
    api::{Api, State, now_ms},
    service, ui, volume,
};

/// What to back up: a volume (a team backup target) or a database the
/// team backs up.
enum What {
    Volume {
        team_id: String,
        target_id: String,
        volume_id: String,
        label: String,
    },
    Database {
        team_id: String,
        id: String,
        label: String,
    },
}

fn team_backups<'a>(state: &'a State, team_id: &str) -> Option<&'a Value> {
    state.0["teamBackups"]
        .as_array()?
        .iter()
        .find(|t| t["teamId"] == json!(team_id))
}

fn all_backup_dbs(state: &State) -> Vec<&Value> {
    state.0["teamBackups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|t| t["databases"].as_array().into_iter().flatten())
        .collect()
}

fn team_slug(state: &State, team_id: &str) -> String {
    state
        .team(team_id)
        .and_then(|t| t["slug"].as_str())
        .unwrap_or("")
        .to_owned()
}

fn no_store(api: &Api, state: &State, team_id: &str) -> anyhow::Error {
    anyhow!(
        "No backup destination for this team. Set one up: {}/{}/backups",
        api.base,
        team_slug(state, team_id)
    )
}

fn volume_target(api: &Api, state: &State, v: &Value) -> Result<What> {
    let team_id = v["teamId"].as_str().unwrap_or("").to_owned();
    let label = volume::path_of(state, v);
    let tb = team_backups(state, &team_id).ok_or_else(|| no_store(api, state, &team_id))?;
    if tb["settings"].is_null() {
        return Err(no_store(api, state, &team_id));
    }
    let Some(t) = tb["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["volumeId"] == v["id"])
    else {
        // A database's own volume: backed up as a database instead.
        if let Some(d) = tb["databases"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| !v["serviceId"].is_null() && d["serviceId"] == v["serviceId"])
        {
            bail!(
                "{label} is a database's: back it up with --db {}",
                d["name"].as_str().unwrap_or("")
            );
        }
        bail!("{label} can't be backed up yet: its service hasn't been deployed.");
    };
    Ok(What::Volume {
        team_id,
        target_id: t["id"].as_str().unwrap_or("").to_owned(),
        volume_id: v["id"].as_str().unwrap_or("").to_owned(),
        label,
    })
}

fn database_target(api: &Api, state: &State, which: &str) -> Result<What> {
    let hits: Vec<&Value> = all_backup_dbs(state)
        .into_iter()
        .filter(|d| {
            d["id"] == json!(which) || d["name"] == json!(which) || d["databaseId"] == json!(which)
        })
        .collect();
    if hits.len() > 1 {
        bail!("More than one database is called {which}: pass its id.");
    }
    if let Some(d) = hits.first() {
        let team_id = state.0["teamBackups"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| {
                t["databases"]
                    .as_array()
                    .is_some_and(|ds| ds.iter().any(|x| x["id"] == d["id"]))
            })
            .and_then(|t| t["teamId"].as_str())
            .unwrap_or("")
            .to_owned();
        if team_backups(state, &team_id).is_none_or(|tb| tb["settings"].is_null()) {
            return Err(no_store(api, state, &team_id));
        }
        return Ok(What::Database {
            team_id,
            id: d["id"].as_str().unwrap_or("").to_owned(),
            label: d["name"].as_str().unwrap_or(which).to_owned(),
        });
    }
    // A built-in database the team doesn't back up this way.
    let built_in = state.0["databases"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["id"] == json!(which) || d["name"] == json!(which));
    let Some(d) = built_in else {
        bail!("No database {which} in your teams.")
    };
    let (name, engine, id) = (
        d["name"].as_str().unwrap_or(which),
        d["engine"].as_str().unwrap_or(""),
        d["id"].as_str().unwrap_or(""),
    );
    let team_id = d["teamId"].as_str().unwrap_or("");
    if engine == "postgres" || engine == "mysql" {
        bail!(
            "{name} is backed up continuously when its backups are on, so there's nothing to run now. See: {}/{}/{engine}/{id}/backups",
            api.base,
            team_slug(state, team_id)
        );
    }
    bail!(
        "Backups are off for {name}. Turn them on: {}/{}/kv/{id}/backups",
        api.base,
        team_slug(state, team_id)
    )
}

/// The linked service's one volume or database, or a list to pick with a flag.
fn default_target(api: &Api, t: &service::Target) -> Result<What> {
    let state = &t.state;
    let svc = t.service();
    let team_id = svc["teamId"].as_str().unwrap_or("");
    let tb = team_backups(state, team_id);
    let targets: Vec<&Value> = tb
        .map(|tb| {
            tb["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|x| x["serviceId"] == json!(t.id))
                .collect()
        })
        .unwrap_or_default();
    let dbs: Vec<&Value> = tb
        .map(|tb| {
            tb["databases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|x| x["serviceId"] == json!(t.id))
                .collect()
        })
        .unwrap_or_default();
    let vols: Vec<&Value> = volume::volumes(state)
        .iter()
        .filter(|v| v["serviceId"] == json!(t.id) && v["deleting"] != json!(true))
        .collect();
    let name = state.path_of(svc);
    match (targets.len() + dbs.len(), vols.len()) {
        (1, _) if dbs.len() == 1 => {
            database_target(api, state, dbs[0]["id"].as_str().unwrap_or(""))
        }
        (1, _) => {
            let v = vols
                .iter()
                .find(|v| v["id"] == targets[0]["volumeId"])
                .ok_or_else(|| anyhow!("{name}'s volume is gone."))?;
            volume_target(api, state, v)
        }
        (0, 0) => bail!("{name} has no volumes to back up."),
        (0, 1) => volume_target(api, state, vols[0]),
        _ => {
            println!("{}", ui::bold(&format!("{name} has:")));
            for v in &vols {
                println!("  volume {}", v["name"].as_str().unwrap_or(""));
            }
            for d in &dbs {
                println!("  database {}", d["name"].as_str().unwrap_or(""));
            }
            bail!("Say which: --volume NAME or --db NAME.")
        }
    }
}

fn size(bytes: Option<f64>) -> String {
    match bytes {
        Some(b) if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        Some(b) if b >= 1e6 => format!("{:.1} MB", b / 1e6),
        Some(b) => format!("{:.0} KB", b / 1e3),
        None => "—".into(),
    }
}

fn took(run: &Value) -> String {
    let start = run["startedAt"]
        .as_i64()
        .or(run["createdAt"].as_i64())
        .unwrap_or(0);
    let s = (run["finishedAt"].as_i64().unwrap_or(start) - start).max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

pub async fn now(
    api: &Api,
    service: Option<&str>,
    volume_name: Option<String>,
    db: Option<String>,
    wait: bool,
) -> Result<()> {
    let what = match (volume_name, db) {
        (Some(v), _) => {
            let state = api.state().await?;
            let v = volume::resolve(&state, &v)?;
            volume_target(api, &state, &v)?
        }
        (None, Some(d)) => database_target(api, &api.state().await?, &d)?,
        (None, None) => {
            let t = service::target(api, service).await?;
            default_target(api, &t)?
        }
    };
    match what {
        What::Volume {
            team_id,
            target_id,
            volume_id,
            label,
        } => {
            let state = api.state().await?;
            let before: Vec<Value> = team_backups(&state, &team_id)
                .and_then(|tb| tb["history"].as_array())
                .into_iter()
                .flatten()
                .map(|h| h["id"].clone())
                .collect();
            api.op(
                "runTeamBackup",
                json!({ "teamId": team_id, "target": target_id }),
            )
            .await?;
            ui::ok(&format!("Backing up {label}"));
            if !wait {
                println!(
                    "{}",
                    ui::dim(&format!("See it: pyo volume snapshots {label}"))
                );
                return Ok(());
            }
            follow(api, &label, |state| {
                team_backups(state, &team_id)?["history"]
                    .as_array()?
                    .iter()
                    .find(|h| h["volumeId"] == json!(volume_id) && !before.contains(&h["id"]))
                    .cloned()
            })
            .await
        }
        What::Database { team_id, id, label } => {
            let r = api.op("runDatabaseBackup", json!({ "id": id })).await?;
            let run_id = r["id"].clone();
            ui::ok(&format!("Backing up {label}"));
            if !wait {
                let state = api.state().await?;
                println!(
                    "{}",
                    ui::dim(&format!(
                        "See it: {}/{}/backups",
                        api.base,
                        team_slug(&state, &team_id)
                    ))
                );
                return Ok(());
            }
            follow(api, &label, |state| {
                all_backup_dbs(state)
                    .into_iter()
                    .flat_map(|d| d["runs"].as_array().into_iter().flatten())
                    .find(|x| x["id"] == run_id)
                    .cloned()
            })
            .await
        }
    }
}

/// Poll until the run finishes; fails when it does.
async fn follow(api: &Api, label: &str, find: impl Fn(&State) -> Option<Value>) -> Result<()> {
    let started = now_ms();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let state = api.state().await?;
        match find(&state) {
            Some(run) if run["status"] == json!("succeeded") => {
                ui::ok(&format!(
                    "Backed up {label}: {} in {}",
                    size(run["sizeBytes"].as_f64()),
                    took(&run)
                ));
                return Ok(());
            }
            Some(run) if run["status"] == json!("failed") => bail!(
                "Backup of {label} failed: {}",
                run["error"].as_str().unwrap_or("no reason given")
            ),
            None if now_ms() - started > 60_000 => {
                bail!("The backup didn't show up. Check Backups on the dashboard.")
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_times() {
        assert_eq!(size(Some(12_300_000.0)), "12.3 MB");
        assert_eq!(size(Some(2_500_000_000.0)), "2.5 GB");
        assert_eq!(size(None), "—");
        assert_eq!(
            took(&json!({ "createdAt": 0, "startedAt": 1000, "finishedAt": 15_000 })),
            "14s"
        );
        assert_eq!(
            took(&json!({ "createdAt": 0, "finishedAt": 125_000 })),
            "2m 5s"
        );
    }
}
