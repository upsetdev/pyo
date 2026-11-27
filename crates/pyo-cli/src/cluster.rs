//! `pyo cluster`: a cluster's traffic paths (public ports, Cloudflare
//! Tunnel, Tailnet), through `setClusterPaths`.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::{api::Api, ui};

/// "on" or "off".
pub fn on_off(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        _ => bail!("Use on or off, not {v}."),
    }
}

/// What to change, as the flags say.
#[derive(Debug, Default)]
pub struct Change {
    pub public: Option<bool>,
    pub tunnel: Option<bool>,
    pub tailnet: Option<bool>,
    pub ports: Option<bool>,
    pub move_to_tunnel: Option<bool>,
    pub restore_records: Option<bool>,
}

impl Change {
    fn is_empty(&self) -> bool {
        self.public.is_none() && self.tunnel.is_none() && self.tailnet.is_none() && self.ports.is_none()
    }

    fn args(&self, cluster_id: &str, confirm: bool) -> Value {
        let mut a = json!({ "clusterId": cluster_id, "confirm": confirm });
        for (k, v) in [
            ("public", self.public),
            ("tunnel", self.tunnel),
            ("tailnet", self.tailnet),
            ("publicPorts", self.ports),
            ("moveToTunnel", self.move_to_tunnel),
            ("restoreRecords", self.restore_records),
        ] {
            if let Some(v) = v {
                a[k] = json!(v);
            }
        }
        a
    }
}

/// The paths as one line each: "Public ports  on".
pub fn paths_lines(c: &Value) -> Vec<String> {
    let p = &c["paths"];
    let word = |v: &Value| if v == &json!(true) { "on" } else { "off" };
    let mut out = vec![
        format!("Public ports       {}", word(&p["public"])),
        format!("Cloudflare Tunnel  {}", word(&p["tunnel"])),
        format!("Tailnet            {}", word(&p["tailnet"])),
    ];
    if p["public"] != json!(true) {
        out.push(format!("TCP and UDP ports  {}", word(&c["publicPorts"])));
    }
    if c["closingPublic"] == json!(true) {
        out.push("Public ports close once the domains moving to the tunnel are on it.".into());
    }
    out
}

/// A "confirm" answer: what moves and what stops.
pub fn confirm_lines(answer: &Value) -> Vec<String> {
    let mut out = vec![];
    for d in answer["domains"].as_array().into_iter().flatten() {
        let host = d["hostname"].as_str().unwrap_or("?");
        let service = d["service"].as_str().unwrap_or("?");
        let what = match (d["outcome"].as_str(), d["path"].as_str()) {
            (Some("moves"), Some("public")) => "moves to the tunnel",
            (Some("moves"), _) => "moves back to public ports",
            _ => "stops answering",
        };
        out.push(format!("  {host} ({service}) {what}"));
    }
    if answer["publicDashboard"] == json!(true) {
        out.push("  The public dashboard closes.".into());
    }
    out
}

/// `pyo cluster paths <cluster> [--public on|off] …`.
pub async fn paths(api: &Api, team: Option<&str>, cluster: &str, change: Change, yes: bool) -> Result<()> {
    let (_, id) = crate::compose::team_and_cluster(api, team, Some(cluster)).await?;
    let state = api.state().await?;
    let c = state.cluster(&id).cloned().unwrap_or(Value::Null);
    let name = c["name"].as_str().unwrap_or(&id).to_owned();
    if change.is_empty() {
        println!("{name}");
        for l in paths_lines(&c) {
            println!("  {l}");
        }
        return Ok(());
    }
    let r = api.op("setClusterPaths", change.args(&id, yes)).await?;
    if r["status"] == json!("confirm") {
        println!("Turning this off on {name} touches these domains:");
        for l in confirm_lines(&r) {
            println!("{l}");
        }
        println!("Run again with --yes to go ahead.");
        std::process::exit(1);
    }
    match r["status"].as_str() {
        Some("moving") => ui::ok(&format!(
            "{name}: domains are moving. Public ports close or the tunnel goes once they're done."
        )),
        _ => ui::ok(&format!("{name}: traffic paths saved")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_read_on_and_off() {
        assert!(on_off("on").unwrap());
        assert!(!on_off("OFF").unwrap());
        assert_eq!(on_off("maybe").unwrap_err().to_string(), "Use on or off, not maybe.");
    }

    #[test]
    fn only_the_flags_given_are_sent() {
        let c = Change { public: Some(false), ports: Some(true), ..Default::default() };
        assert_eq!(
            c.args("cl_1", false),
            json!({ "clusterId": "cl_1", "confirm": false, "public": false, "publicPorts": true })
        );
        assert!(Change::default().is_empty());
    }

    #[test]
    fn a_confirm_says_what_moves_and_what_stops() {
        let a = json!({ "status": "confirm", "publicDashboard": true, "domains": [
            { "hostname": "shop.example.com", "service": "web", "path": "public", "outcome": "moves" },
            { "hostname": "old.example.org", "service": "web", "path": "public", "outcome": "stops" },
            { "hostname": "api.example.com", "service": "api", "path": "tunnel", "outcome": "moves" },
        ] });
        assert_eq!(
            confirm_lines(&a),
            [
                "  shop.example.com (web) moves to the tunnel",
                "  old.example.org (web) stops answering",
                "  api.example.com (api) moves back to public ports",
                "  The public dashboard closes.",
            ]
        );
    }

    #[test]
    fn the_paths_show_one_per_line() {
        let c = json!({ "paths": { "public": false, "tunnel": true, "tailnet": true }, "publicPorts": true });
        assert_eq!(
            paths_lines(&c),
            ["Public ports       off", "Cloudflare Tunnel  on", "Tailnet            on", "TCP and UDP ports  on"]
        );
    }
}
