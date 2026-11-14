//! `pyo firewall`: who may reach a service. Its access rules (checked in
//! order, the first match decides), rate limits, and whether its websites
//! are behind a CDN.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::{
    api::{Api, new_id},
    service, ui,
};

/// Presets a rule can list next to addresses: (name, label).
pub const PRESETS: [(&str, &str); 6] = [
    ("cloudflare", "Cloudflare"),
    ("fastly", "Fastly"),
    ("bunny", "bunny.net"),
    ("cloudfront", "Amazon CloudFront"),
    ("tailnet", "Tailnet"),
    ("private", "Private networks"),
];

fn label(source: &str) -> String {
    PRESETS
        .iter()
        .find(|(n, _)| *n == source)
        .map(|(_, l)| (*l).to_owned())
        .unwrap_or_else(|| match source.split_once('/') {
            Some((a, "32")) if !a.contains(':') => a.to_owned(),
            Some((a, "128")) => a.to_owned(),
            _ => source.to_owned(),
        })
}

/// A rule in words: "Block if client IP is not in Cloudflare".
pub fn describe(r: &Value) -> String {
    let action = match r["action"].as_str() {
        Some("allow") => "Allow".to_owned(),
        Some("block") => "Block".to_owned(),
        _ => format!(
            "Rate limit to {} a {}",
            r["requests"],
            r["per"].as_str().unwrap_or("minute")
        ),
    };
    let sources: Vec<&str> = r["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if sources.is_empty() {
        return match r["action"].as_str() {
            Some("allow" | "block") => format!("{action} any client"),
            _ => format!(
                "Rate limit any client to {} a {}",
                r["requests"],
                r["per"].as_str().unwrap_or("minute")
            ),
        };
    }
    let field = if sources
        .iter()
        .any(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_uppercase()))
    {
        "country"
    } else {
        "client IP"
    };
    let op = if r["operator"] == "notIn" {
        "is not in"
    } else {
        "is in"
    };
    let list: Vec<String> = sources.iter().map(|s| label(s)).collect();
    format!("{action} if {field} {op} {}", list.join(", "))
}

fn rules_of(t: &service::Target) -> Vec<Value> {
    let rules: Vec<Value> = t.state.0["firewallRules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["serviceId"] == json!(t.id))
        .cloned()
        .collect();
    // In the order they're checked: allow and block rules, then rate limits.
    let (limits, access): (Vec<Value>, Vec<Value>) =
        rules.into_iter().partition(|r| r["action"] == "rateLimit");
    access.into_iter().chain(limits).collect()
}

pub async fn list(api: &Api, service: Option<&str>) -> Result<()> {
    let t = service::target(api, service).await?;
    let behind = t.service()["behindCdn"].as_str().map(label);
    let rules = rules_of(&t);
    if behind.is_none() && rules.is_empty() {
        println!("No rules: everyone gets in. Add one: pyo firewall add block 203.0.113.7");
        return Ok(());
    }
    let mut n = 0;
    if let Some(cdn) = &behind {
        n += 1;
        println!(
            "{n:>2}  Block if the connection is not from {cdn}  {}",
            ui::dim(&format!("Added by Behind {cdn}"))
        );
    }
    for r in &rules {
        let what = describe(r);
        let note = r["note"].as_str().filter(|n| !n.is_empty() && *n != what);
        let id = r["id"].as_str().unwrap_or("");
        if r["action"] == "rateLimit" {
            print!("    ");
        } else {
            n += 1;
            print!("{n:>2}  ");
        }
        match note {
            Some(note) => println!("{what}  {}", ui::dim(&format!("{note} · {id}"))),
            None => println!("{what}  {}", ui::dim(id)),
        }
    }
    Ok(())
}

pub struct NewRule {
    pub action: String,
    pub not: bool,
    pub sources: Vec<String>,
    pub requests: Option<u32>,
    pub per: Option<String>,
    pub name: Option<String>,
}

pub async fn add(api: &Api, service: Option<&str>, r: NewRule) -> Result<()> {
    let t = service::target(api, service).await?;
    let action = match r.action.as_str() {
        "allow" | "block" => r.action.clone(),
        "rate-limit" | "ratelimit" | "rateLimit" => "rateLimit".to_owned(),
        a => bail!("{a:?}: allow, block or rate-limit."),
    };
    if action == "rateLimit" && r.requests.is_none() {
        bail!("A rate limit needs --requests.");
    }
    let mut args = json!({
        "id": new_id("fwr"),
        "serviceId": t.id,
        "action": action,
        "operator": if r.not { "notIn" } else { "in" },
        "sources": r.sources,
        "note": r.name.clone().unwrap_or_default(),
    });
    if let Some(n) = r.requests {
        args["requests"] = json!(n);
        args["per"] = json!(r.per.clone().unwrap_or_else(|| "minute".into()));
    }
    api.op("addFirewallRule", args.clone()).await?;
    ui::ok(&format!("Added: {}", describe(&args)));
    Ok(())
}

pub async fn remove(api: &Api, service: Option<&str>, which: &str, yes: bool) -> Result<()> {
    let t = service::target(api, service).await?;
    let rules = rules_of(&t);
    let offset = usize::from(t.service()["behindCdn"].is_string());
    let rule = match which.parse::<usize>() {
        Ok(n) if n > offset => rules
            .iter()
            .filter(|r| r["action"] != "rateLimit")
            .nth(n - 1 - offset),
        Ok(_) if offset == 1 => bail!("That one is Behind Cloudflare's: pyo firewall behind off"),
        _ => rules.iter().find(|r| r["id"] == json!(which)),
    };
    let Some(rule) = rule else {
        bail!("No rule {which}. See them: pyo firewall list");
    };
    let what = describe(rule);
    if !yes && !ui::ask(&format!("Remove \"{what}\"? [y/N] "))?.eq_ignore_ascii_case("y") {
        bail!("Not removed.");
    }
    api.op("removeFirewallRule", json!({ "id": rule["id"] }))
        .await?;
    ui::ok(&format!("Removed: {what}"));
    Ok(())
}

pub async fn behind(api: &Api, service: Option<&str>, cdn: &str) -> Result<()> {
    let t = service::target(api, service).await?;
    let cdn = match cdn {
        "off" | "none" => None,
        c => Some(c.to_owned()),
    };
    api.op(
        "setServiceBehindCdn",
        json!({ "serviceId": t.id, "cdn": cdn }),
    )
    .await?;
    let name = t.service()["name"].as_str().unwrap_or("").to_owned();
    match cdn {
        Some(c) => ui::ok(&format!(
            "{name} is behind {}: only it reaches the service's websites",
            label(&c)
        )),
        None => ui::ok(&format!("{name} is no longer behind a CDN")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_in_words() {
        assert_eq!(
            describe(
                &json!({ "action": "block", "operator": "notIn", "sources": ["cloudflare", "203.0.113.7/32"] })
            ),
            "Block if client IP is not in Cloudflare, 203.0.113.7"
        );
        assert_eq!(
            describe(&json!({ "action": "allow", "operator": "in", "sources": ["DE"] })),
            "Allow if country is in DE"
        );
        assert_eq!(
            describe(
                &json!({ "action": "rateLimit", "operator": "in", "sources": [], "requests": 10, "per": "second" })
            ),
            "Rate limit any client to 10 a second"
        );
    }
}
