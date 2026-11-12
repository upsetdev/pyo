//! `pyo registry login`: log Docker in to Pyo registry with a fresh token,
//! to push the team's own images (<team>/<project>/<service>). Pyo registry
//! is on the installation's tailnet (HTTPS on its tailnet name), so this only
//! works from a device on it.

use std::{io::Write, process::Stdio};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::api::Api;

pub async fn login(api: &Api, team: Option<&str>, pull_only: bool) -> Result<()> {
    let state = api.state().await?;
    let teams = state.teams();
    let team = match team {
        Some(t) => teams
            .iter()
            .find(|x| x["slug"] == json!(t) || x["id"] == json!(t))
            .with_context(|| format!("No team {t} among yours"))?,
        None => teams.first().context("You aren't in any team")?,
    };
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this computer".into());
    // A new id the API accepts: rtk_ and lowercase letters and digits.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id = format!("rtk_cli{}", nanos % 10u128.pow(20));
    let r = api
        .op(
            "createRegistryToken",
            json!({ "id": id, "teamId": team["id"], "name": format!("CLI on {host}"), "scope": if pull_only { "pull" } else { "push" }, "expiresInDays": 30 }),
        )
        .await?;
    let Some(address) = r["address"].as_str() else {
        bail!(
            "This Pyo isn't on its tailnet over HTTPS yet, so its registry can't be reached from here. On its machine, turn on Tailscale with MagicDNS and HTTPS certificates, then run its setup again."
        );
    };
    let (user, token, path) = (
        r["username"].as_str().unwrap_or(""),
        r["token"].as_str().unwrap_or(""),
        r["path"].as_str().unwrap_or(""),
    );
    let mut docker = std::process::Command::new("docker")
        .args(["login", address, "-u", user, "--password-stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .context("Couldn't run docker: is it installed?")?;
    docker
        .stdin
        .take()
        .context("docker's input")?
        .write_all(token.as_bytes())?;
    let status = docker.wait()?;
    if !status.success() {
        bail!("docker login didn't work. Is this device on the tailnet?");
    }
    println!("Logged in to {address} for 30 days. Push with:");
    println!("  docker tag <image> {address}/{path}/<project>/<service>:<tag>");
    println!("  docker push {address}/{path}/<project>/<service>:<tag>");
    Ok(())
}
