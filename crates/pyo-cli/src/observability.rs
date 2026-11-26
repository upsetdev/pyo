//! `pyo observability --team`: a team's observability. Whether its servers
//! collect analytics, request logs and usage history, whether they keep its
//! request logs, and what they keep of visitors' IP addresses and query
//! strings, up to the installation's limits.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::{api::Api, ui};

/// What to change. None: leave it.
pub struct Change {
    pub state: Option<String>,
    pub request_logs: Option<String>,
    pub ip: Option<String>,
    pub query_strings: Option<String>,
}

impl Change {
    /// The op's fields, or None when nothing changes.
    fn fields(&self) -> Option<Map<String, Value>> {
        let mut m = Map::new();
        if let Some(s) = &self.state {
            m.insert("enabled".into(), json!(s == "on"));
        }
        if let Some(s) = &self.request_logs {
            m.insert("requestLogs".into(), json!(s == "on"));
        }
        if let Some(s) = &self.ip {
            m.insert("ipMode".into(), json!(s));
        }
        if let Some(s) = &self.query_strings {
            m.insert("queryStrings".into(), json!(s));
        }
        (!m.is_empty()).then_some(m)
    }
}

fn ip_label(m: &str) -> &str {
    match m {
        "none" => "don't store",
        other => other,
    }
}

fn query_label(q: &str) -> &str {
    match q {
        "names" => "names only",
        "none" => "don't keep",
        other => other,
    }
}

/// The team's settings as lines to print.
fn describe(t: &Value) -> Vec<String> {
    let now = &t["observability"];
    let limits = &t["observabilityLimits"];
    let name = t["name"].as_str().unwrap_or("");
    let on = now["on"].as_bool().unwrap_or(true);
    let mut out = vec![format!("Observability is {} for {name}.", if on { "on" } else { "off" })];
    out.push(format!("Request logs: {}", if now["requestLogs"].as_bool().unwrap_or(true) { "on" } else { "off" }));
    let ip = now["ipMode"].as_str().unwrap_or("none");
    let ip_limit = limits["ipMode"].as_str().unwrap_or(ip);
    out.push(format!("IP addresses: {} (limit: {})", ip_label(ip), ip_label(ip_limit)));
    let qs = now["queryStrings"].as_str().unwrap_or("keep");
    let qs_limit = limits["queryStrings"].as_str().unwrap_or(qs);
    out.push(format!("Query strings: {} (limit: {})", query_label(qs), query_label(qs_limit)));
    out
}

pub async fn team(api: &Api, team: &str, change: Change) -> Result<()> {
    let find = |state: &crate::api::State| {
        state
            .teams()
            .iter()
            .find(|x| x["slug"] == json!(team) || x["id"] == json!(team))
            .cloned()
            .with_context(|| format!("No team {team} among yours"))
    };
    let t = find(&api.state().await?)?;
    if let Some(mut fields) = change.fields() {
        fields.insert("teamId".into(), t["id"].clone());
        api.op("setTeamObservability", Value::Object(fields)).await?;
    }
    let t = find(&api.state().await?)?;
    if t["observability"].is_null() {
        println!("{}", ui::dim("This Pyo doesn't have team observability settings yet. Update it first."));
        return Ok(());
    }
    for line in describe(&t) {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_team_in_words() {
        let t = json!({
            "name": "Shop",
            "observability": { "on": true, "requestLogs": false, "ipMode": "none", "queryStrings": "names" },
            "observabilityLimits": { "ipMode": "anonymized", "queryStrings": "keep" },
        });
        assert_eq!(
            describe(&t),
            [
                "Observability is on for Shop.",
                "Request logs: off",
                "IP addresses: don't store (limit: anonymized)",
                "Query strings: names only (limit: keep)",
            ]
        );
    }

    #[test]
    fn only_what_was_asked_changes() {
        let none = Change { state: None, request_logs: None, ip: None, query_strings: None };
        assert!(none.fields().is_none());
        let c = Change { state: Some("off".into()), request_logs: None, ip: Some("default".into()), query_strings: None };
        assert_eq!(Value::Object(c.fields().unwrap()), json!({ "enabled": false, "ipMode": "default" }));
    }
}
