//! `pyo cluster`: a cluster's traffic paths (public ports, Cloudflare
//! Tunnel, Tailnet), through `setClusterPaths`, and its gateway settings,
//! through `setClusterGateway`.

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

/// What to change in the gateway, as the flags say.
#[derive(Debug, Default)]
pub struct GatewayChange {
    pub http3: Option<bool>,
    pub compression: Option<bool>,
    pub tls_min: Option<String>,
    pub request_timeout: Option<u32>,
    pub idle_timeout: Option<u32>,
}

impl GatewayChange {
    fn args(&self, cluster_id: &str) -> Value {
        let mut a = json!({ "clusterId": cluster_id });
        if let Some(v) = self.http3 {
            a["http3"] = json!(v);
        }
        if let Some(v) = self.compression {
            a["compression"] = json!(v);
        }
        if let Some(v) = &self.tls_min {
            a["tlsMin"] = json!(v);
        }
        if let Some(v) = self.request_timeout {
            a["requestTimeout"] = json!(v);
        }
        if let Some(v) = self.idle_timeout {
            a["idleTimeout"] = json!(v);
        }
        a
    }

    fn is_empty(&self) -> bool {
        self.args("").as_object().is_some_and(|o| o.len() == 1)
    }

    /// What saving it drops, against the saved settings, in plain words.
    /// Empty: nothing drops.
    pub fn drops(&self, saved: &Value, cluster: &str) -> Vec<String> {
        let mut out = vec![];
        if self.http3.is_some_and(|v| saved["http3"] != json!(v)) {
            out.push("Open connections on each server's public ports drop once, one server at a time.".into());
        }
        if self.tls_min.as_deref().is_some_and(|v| saved["tlsMin"] != json!(v.trim())) {
            out.push(format!("Open HTTPS connections on every website of {cluster} drop once."));
        }
        out
    }
}

/// The gateway's settings and status, one line each.
pub fn gateway_lines(g: &Value) -> Vec<String> {
    let s = &g["settings"];
    let word = |v: &Value| if v == &json!(true) { "on" } else { "off" };
    let request = match s["requestTimeout"].as_u64() {
        Some(0) => "no limit".to_owned(),
        Some(n) => format!("{n} s"),
        None => "300 s".to_owned(),
    };
    let mut out = vec![
        "HTTP/2             on".to_owned(),
        format!("HTTP/3             {}", word(&s["http3"])),
        format!("Compression        {}", word(&s["compression"])),
        format!("TLS minimum        {}", s["tlsMin"].as_str().unwrap_or("1.2")),
        format!("Request timeout    {request} (services without their own)"),
        format!("Idle timeout       {} s (services without their own)", s["idleTimeout"].as_u64().unwrap_or(3600)),
    ];
    let st = &g["status"];
    if !st.is_object() {
        out.push("Status             not reported yet".into());
        return out;
    }
    let part = |p: &Value| {
        let mut w = vec![if p["ready"] == json!(true) { "ready".to_owned() } else { "not ready".to_owned() }];
        if let Some(v) = p["version"].as_str().filter(|v| !v.is_empty()) {
            w.push(v.to_owned());
        }
        if p["ready"] != json!(true)
            && let Some(r) = p["reason"].as_str()
        {
            w.push(r.to_owned());
        }
        w.join(", ")
    };
    out.push(format!("Controller         {}", part(&st["controller"])));
    for p in st["proxies"].as_array().into_iter().flatten() {
        out.push(format!("Proxy on {}  {}", p["server"].as_str().unwrap_or("?"), part(p)));
    }
    for p in st["firewalls"].as_array().into_iter().flatten() {
        out.push(format!("Firewall on {}  {}", p["server"].as_str().unwrap_or("?"), part(p)));
    }
    out
}

/// `pyo cluster gateway <cluster> [--http3 on|off] …`.
pub async fn gateway(api: &Api, team: Option<&str>, cluster: &str, change: GatewayChange, yes: bool) -> Result<()> {
    let (_, id) = crate::compose::team_and_cluster(api, team, Some(cluster)).await?;
    let state = api.state().await?;
    let c = state.cluster(&id).cloned().unwrap_or(Value::Null);
    let name = c["name"].as_str().unwrap_or(&id).to_owned();
    if change.is_empty() {
        println!("{name}");
        for l in gateway_lines(&c["gateway"]) {
            println!("  {l}");
        }
        return Ok(());
    }
    let drops = change.drops(&c["gateway"]["settings"], &name);
    if !drops.is_empty() && !yes {
        for l in &drops {
            println!("{l}");
        }
        println!("Visitors reconnect. WebSockets and uploads in progress are cut.");
        println!("Run again with --yes to go ahead.");
        std::process::exit(1);
    }
    let r = api.op("setClusterGateway", change.args(&id)).await?;
    if r["drops"] == json!(true) {
        ui::ok(&format!("{name}: gateway saved. Open connections drop once."));
    } else {
        ui::ok(&format!("{name}: gateway saved"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_flags_send_only_whats_given() {
        let c = GatewayChange { compression: Some(true), idle_timeout: Some(7200), ..Default::default() };
        assert_eq!(c.args("cl_1"), json!({ "clusterId": "cl_1", "compression": true, "idleTimeout": 7200 }));
        assert!(GatewayChange::default().is_empty());
        assert!(!c.is_empty());
    }

    #[test]
    fn only_http3_and_the_tls_minimum_ask_first() {
        let saved = json!({ "http3": false, "compression": false, "tlsMin": "1.2", "requestTimeout": 300, "idleTimeout": 3600 });
        let routes_only = GatewayChange { compression: Some(true), request_timeout: Some(60), ..Default::default() };
        assert!(routes_only.drops(&saved, "eu").is_empty());
        // The same as saved: nothing drops.
        assert!(GatewayChange { http3: Some(false), tls_min: Some("1.2".into()), ..Default::default() }.drops(&saved, "eu").is_empty());
        assert_eq!(
            GatewayChange { http3: Some(true), tls_min: Some("1.3".into()), ..Default::default() }.drops(&saved, "eu"),
            [
                "Open connections on each server's public ports drop once, one server at a time.",
                "Open HTTPS connections on every website of eu drop once.",
            ]
        );
    }

    #[test]
    fn the_gateway_shows_one_line_each() {
        let g = json!({
            "settings": { "http3": true, "compression": false, "tlsMin": "1.3", "requestTimeout": 0, "idleTimeout": 7200 },
            "status": {
                "controller": { "ready": true, "version": "1.9.3" },
                "proxies": [{ "server": "server-1", "ready": false, "version": "1.39.5", "restarts": 2, "reason": "Starting." }],
                "firewalls": [],
            },
        });
        assert_eq!(
            gateway_lines(&g),
            [
                "HTTP/2             on",
                "HTTP/3             on",
                "Compression        off",
                "TLS minimum        1.3",
                "Request timeout    no limit (services without their own)",
                "Idle timeout       7200 s (services without their own)",
                "Controller         ready, 1.9.3",
                "Proxy on server-1  not ready, 1.39.5, Starting.",
            ]
        );
        assert_eq!(gateway_lines(&json!({ "settings": {} })).last().unwrap(), "Status             not reported yet");
    }

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
