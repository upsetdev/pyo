//! `pyo volume`: list a team's volumes, their backups (snapshots), restore
//! one or delete one, grow one, and attach one to a service (in another
//! project too, on the same cluster: Pyo moves it there first).

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::{
    api::{Api, State},
    service, ui,
};

pub fn volumes(state: &State) -> &[Value] {
    state.0["volumes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// "project/name" (or "Deleted project/name").
pub fn path_of(state: &State, v: &Value) -> String {
    let project = state
        .project(v["projectId"].as_str().unwrap_or(""))
        .and_then(|p| p["name"].as_str())
        .unwrap_or("(deleted project)");
    format!("{project}/{}", v["name"].as_str().unwrap_or("?"))
}

/// A volume by id, "name" or "project/name".
pub fn resolve(state: &State, s: &str) -> Result<Value> {
    let s = s.trim_matches('/');
    let all = volumes(state);
    if let Some(v) = all.iter().find(|v| v["id"] == json!(s)) {
        return Ok(v.clone());
    }
    let hits: Vec<&Value> = all
        .iter()
        .filter(|v| v["deleting"] != json!(true))
        .filter(|v| v["name"] == json!(s) || path_of(state, v) == s)
        .collect();
    match hits.len() {
        0 => bail!("No volume called \"{s}\". See them all: pyo volume list"),
        1 => Ok(hits[0].clone()),
        _ => {
            let names: Vec<String> = hits.iter().map(|v| path_of(state, v)).collect();
            let i = ui::choose(&format!("More than one volume is called \"{s}\":"), &names)?;
            Ok(hits[i].clone())
        }
    }
}

fn status(state: &State, v: &Value) -> String {
    if v["deleting"] == json!(true) {
        return "deleting".into();
    }
    if !v["restore"].is_null() {
        return "restoring".into();
    }
    if !v["moving"].is_null() {
        return "moving".into();
    }
    if let Some(n) = v["resizeTo"].as_i64() {
        return format!("growing to {n} GB");
    }
    match v["serviceId"]
        .as_str()
        .and_then(|id| state.service(id).ok())
    {
        Some(a) => format!(
            "{} at {}",
            a["name"].as_str().unwrap_or("?"),
            v["mountPath"].as_str().unwrap_or("")
        ),
        None => "not attached".into(),
    }
}

pub async fn list(api: &Api) -> Result<()> {
    let state = api.state().await?;
    let mut all: Vec<&Value> = volumes(&state).iter().collect();
    if all.is_empty() {
        println!("No volumes.");
        return Ok(());
    }
    all.sort_by_key(|v| path_of(&state, v));
    for v in all {
        println!(
            "{}  {} GB  {}  {}",
            ui::bold(&path_of(&state, v)),
            v["sizeGi"],
            status(&state, v),
            ui::dim(v["id"].as_str().unwrap_or(""))
        );
        for (k, label) in [
            ("restoreError", "restore failed"),
            ("moveError", "move failed"),
            ("resizeError", "resize failed"),
        ] {
            if let Some(e) = v[k].as_str() {
                println!("  {} {e}", ui::red(&format!("{label}:")));
            }
        }
    }
    Ok(())
}

async fn snapshot_list(api: &Api, id: &str) -> Result<Vec<Value>> {
    eprintln!("{}", ui::dim("Reading backups from the cluster…"));
    let r = api.get(&format!("/api/v1/volumes/{id}/snapshots")).await?;
    Ok(r["snapshots"].as_array().cloned().unwrap_or_default())
}

fn describe(s: &Value) -> String {
    let size = s["sizeBytes"]
        .as_f64()
        .map(|b| format!("{:.1} MB", b / 1e6))
        .unwrap_or_else(|| "—".into());
    format!(
        "{}  {}  {}",
        s["shortId"]
            .as_str()
            .unwrap_or(s["id"].as_str().unwrap_or("")),
        ui::ago(s["time"].as_i64().unwrap_or(0)),
        size
    )
}

pub async fn snapshots(api: &Api, volume: &str) -> Result<()> {
    let state = api.state().await?;
    let v = resolve(&state, volume)?;
    let list = snapshot_list(api, v["id"].as_str().unwrap_or("")).await?;
    if list.is_empty() {
        println!("No backups of {} yet.", path_of(&state, &v));
    }
    for s in &list {
        println!("{}", describe(s));
    }
    Ok(())
}

/// Restore a snapshot (by id; picked from a list without one) and follow
/// it to the end.
pub async fn restore(
    api: &Api,
    volume: &str,
    snapshot: Option<String>,
    yes: bool,
    detach: bool,
) -> Result<()> {
    let state = api.state().await?;
    let v = resolve(&state, volume)?;
    let id = v["id"].as_str().unwrap_or("").to_owned();
    let label = path_of(&state, &v);
    let snapshot = match snapshot {
        Some(s) => s,
        None => {
            let list = snapshot_list(api, &id).await?;
            if list.is_empty() {
                bail!("No backups of {label} yet.");
            }
            let items: Vec<String> = list.iter().map(describe).collect();
            let i = ui::choose(&format!("Restore {label} from:"), &items)?;
            list[i]["id"].as_str().unwrap_or("").to_owned()
        }
    };
    if !yes {
        let service = v["serviceId"]
            .as_str()
            .and_then(|s| state.service(s).ok())
            .and_then(|a| a["name"].as_str())
            .map(|n| format!(" {n} stops while it runs and starts again after."));
        let a = ui::ask(&format!(
            "Replace the data in {label} with snapshot {}?{} [y/N] ",
            &snapshot[..snapshot.len().min(8)],
            service.unwrap_or_default()
        ))?;
        if !a.eq_ignore_ascii_case("y") {
            bail!("Not restored.");
        }
    }
    api.op("restoreVolume", json!({ "id": id, "snapshotId": snapshot }))
        .await?;
    if detach {
        ui::ok(&format!("Restoring {label}"));
        return Ok(());
    }
    eprintln!("{}", ui::dim(&format!("Restoring {label}…")));
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let state = api.state().await?;
        let Some(now) = volumes(&state).iter().find(|x| x["id"] == json!(id)) else {
            bail!("{label} is gone.")
        };
        if now["restore"].is_null() {
            if let Some(e) = now["restoreError"].as_str() {
                bail!("Couldn't restore {label}: {e}");
            }
            ui::ok(&format!("Restored {label}"));
            return Ok(());
        }
    }
}

/// Delete one of a volume's backups (by snapshot id; picked from a list
/// without one). Owners and admins.
pub async fn delete_backup(
    api: &Api,
    volume: &str,
    snapshot: Option<String>,
    yes: bool,
) -> Result<()> {
    let state = api.state().await?;
    let v = resolve(&state, volume)?;
    let id = v["id"].as_str().unwrap_or("").to_owned();
    let label = path_of(&state, &v);
    let snapshot = match snapshot {
        Some(s) => s,
        None => {
            let list = snapshot_list(api, &id).await?;
            if list.is_empty() {
                bail!("No backups of {label} yet.");
            }
            let items: Vec<String> = list.iter().map(describe).collect();
            let i = ui::choose(&format!("Delete which backup of {label}?"), &items)?;
            list[i]["id"].as_str().unwrap_or("").to_owned()
        }
    };
    let short = &snapshot[..snapshot.len().min(8)];
    if !yes {
        let a = ui::ask(&format!(
            "Delete backup {short} of {label} for good? [y/N] "
        ))?;
        if !a.eq_ignore_ascii_case("y") {
            bail!("Not deleted.");
        }
    }
    eprintln!("{}", ui::dim(&format!("Deleting backup {short}…")));
    api.delete(&format!("/api/v1/volumes/{id}/snapshots/{snapshot}"))
        .await?;
    ui::ok(&format!("Deleted backup {short} of {label}"));
    Ok(())
}

pub async fn resize(api: &Api, volume: &str, size_gb: i64) -> Result<()> {
    let state = api.state().await?;
    let v = resolve(&state, volume)?;
    api.op("resizeVolume", json!({ "id": v["id"], "sizeGi": size_gb }))
        .await?;
    ui::ok(&format!("Growing {} to {size_gb} GB", path_of(&state, &v)));
    Ok(())
}

/// Attach a volume; one moved to the service's project first is followed
/// until it's there (or the move failed), unless `detach`.
pub async fn attach(
    api: &Api,
    volume: &str,
    service_name: &str,
    path: Option<String>,
    detach: bool,
) -> Result<()> {
    let state = api.state().await?;
    let v = resolve(&state, volume)?;
    let service_id = service::resolve(&state, service_name)?;
    let svc = state.service(&service_id)?;
    let path = path
        .or_else(|| v["mountPath"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "/data".into());
    let r = api
        .op(
            "attachVolume",
            json!({ "id": v["id"], "serviceId": service_id, "mountPath": path }),
        )
        .await?;
    let (label, to) = (path_of(&state, &v), state.path_of(svc));
    if r["moving"] != json!(true) {
        ui::ok(&format!(
            "Attached {label} to {to} at {path}. Deploy to mount it: pyo deploy -s {to}"
        ));
        return Ok(());
    }
    if detach {
        ui::ok(&format!(
            "Moving {label} to {to}'s project. It's attached once moved (see pyo volume list). Then deploy {to}."
        ));
        return Ok(());
    }
    let id = v["id"].as_str().unwrap_or("").to_owned();
    eprintln!("{}", ui::dim(&format!("Moving {label} to {to}'s project…")));
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let state = api.state().await?;
        let Some(now) = volumes(&state).iter().find(|x| x["id"] == json!(id)) else {
            bail!("{label} is gone.")
        };
        if now["moving"].is_null() {
            if let Some(e) = now["moveError"].as_str() {
                bail!("Couldn't move {label}: {e}");
            }
            ui::ok(&format!(
                "Attached {label} to {to} at {path}. Deploy to mount it: pyo deploy -s {to}"
            ));
            return Ok(());
        }
    }
}
