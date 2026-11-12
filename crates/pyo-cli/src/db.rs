//! `pyo db import`: copy a Postgres or MySQL database (by its URL) or a
//! dump file into one of the team's built-in databases, and follow it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{api::Api, ui};

/// The team's database by name or id.
fn find<'a>(state: &'a Value, team: Option<&str>, which: &str) -> Result<&'a Value> {
    let teams = state["teams"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let team_id = match team {
        Some(t) => Some(
            teams
                .iter()
                .find(|x| x["slug"] == json!(t) || x["id"] == json!(t))
                .ok_or_else(|| anyhow!("No team {t} among yours."))?["id"]
                .clone(),
        ),
        None => None,
    };
    let all = state["databases"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut hits = all.iter().filter(|d| {
        (d["id"] == json!(which) || d["name"] == json!(which))
            && team_id.as_ref().is_none_or(|t| &d["teamId"] == t)
    });
    let first = hits
        .next()
        .ok_or_else(|| anyhow!("No database {which} in your teams."))?;
    if hits.next().is_some() {
        bail!("More than one of your teams has a database {which}: say which with --team.");
    }
    Ok(first)
}

/// Upload a dump, as it's read: say what it is, then send it where Pyo
/// says, with only the headers it gives (no API key: the URL may be on
/// another host).
async fn upload(api: &Api, team_id: &str, engine: &str, path: &std::path::Path) -> Result<Value> {
    use tokio::io::AsyncReadExt;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dump.sql")
        .to_owned();
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("Can't read {}", path.display()))?;
    let size = file
        .metadata()
        .await
        .with_context(|| format!("Can't read {}", path.display()))?
        .len();
    let target = api
        .post(
            "/api/v1/database-imports/uploads",
            &json!({ "teamId": team_id, "engine": engine, "filename": name, "size": size }),
        )
        .await?;
    let url = target["uploadUrl"]
        .as_str()
        .ok_or_else(|| anyhow!("Pyo gave no upload URL."))?;
    let url = reqwest::Url::parse(&api.base)
        .and_then(|b| b.join(url))
        .with_context(|| format!("Pyo gave a bad upload URL: {url}"))?;
    let method = reqwest::Method::from_bytes(target["method"].as_str().unwrap_or("PUT").as_bytes())
        .unwrap_or(reqwest::Method::PUT);
    eprintln!(
        "{}",
        ui::dim(&format!(
            "Uploading {name} ({:.1} MiB)…",
            size as f64 / 1_048_576.0
        ))
    );
    let stream = futures::stream::unfold(file, |mut f| async move {
        let mut buf = vec![0u8; 1 << 20];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(buf), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    let mut req = reqwest::Client::builder()
        .user_agent(concat!("pyo-cli/", env!("CARGO_PKG_VERSION")))
        .build()?
        .request(method, url)
        .header("content-length", size);
    for (k, v) in target["headers"].as_object().into_iter().flatten() {
        if let Some(v) = v.as_str() {
            req = req.header(k.as_str(), v);
        }
    }
    let res = req
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .context("The upload broke off")?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("The upload failed ({status})"));
        bail!(msg);
    }
    Ok(target)
}

pub async fn import(
    api: &Api,
    team: Option<&str>,
    which: &str,
    url: Option<String>,
    file: Option<std::path::PathBuf>,
    retry: bool,
    detach: bool,
) -> Result<()> {
    let state = api.state().await?.0;
    let db = find(&state, team, which)?.clone();
    let (id, name, engine, team_id) = (
        db["id"].as_str().unwrap_or(""),
        db["name"].as_str().unwrap_or(""),
        db["engine"].as_str().unwrap_or(""),
        db["teamId"].as_str().unwrap_or(""),
    );
    if engine != "postgres" && engine != "mysql" {
        bail!("{name} is a KV database: imports are into Postgres and MySQL.");
    }
    let started = if retry {
        let import_id = db["import"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("{name} has no import to retry."))?;
        api.post(
            &format!("/api/v1/database-imports/{import_id}/retry"),
            &json!({}),
        )
        .await?
    } else {
        let mut body = json!({ "teamId": team_id, "engine": engine, "databaseId": id });
        match (url, file) {
            (Some(u), None) => body["url"] = json!(u),
            (None, Some(f)) => {
                body["uploadId"] = upload(api, team_id, engine, &f).await?["uploadId"].clone()
            }
            _ => bail!("Give the database to copy (--from, or PYO_IMPORT_URL) or a dump (--file)."),
        }
        eprintln!("{}", ui::dim("Checking…"));
        api.post("/api/v1/database-imports", &body).await?
    };
    let turned_on: Vec<&str> = started["extensions"]["turnedOn"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !turned_on.is_empty() {
        eprintln!(
            "{}",
            ui::dim(&format!("Turning on {} first.", turned_on.join(", ")))
        );
    }
    ui::ok(&format!("Importing into {name}"));
    if detach {
        return Ok(());
    }
    let import_id = started["importId"].as_str().unwrap_or("").to_owned();
    let mut last = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let d = api.get(&format!("/api/v1/databases/{id}")).await?;
        let imp = &d["import"];
        if imp["id"].as_str() != Some(import_id.as_str()) {
            continue;
        }
        let phase = imp["phase"].as_str().unwrap_or("");
        match imp["state"].as_str().unwrap_or("") {
            "succeeded" => {
                ui::ok(&format!("Imported into {name}"));
                return Ok(());
            }
            "failed" => bail!(
                "The import failed: {}\nRetry with: pyo db import {name} --retry",
                imp["message"].as_str().unwrap_or("")
            ),
            _ if !phase.is_empty() && phase != last => {
                eprintln!("{}", ui::dim(phase));
                last = phase.to_owned();
            }
            _ => {}
        }
    }
}
