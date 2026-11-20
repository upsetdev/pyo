//! One gateway line in, one kept row out: the query's sensitive values
//! redacted, the visitor's country looked up and their address hashed with
//! the day's salt, then kept only as the IP setting says. Nothing here
//! leaves the cluster but what the totals add up.

use std::net::IpAddr;

use pyo_proto::traffic::{
    AccessLine, ConnectionRow, RequestRow, TrafficContext, WafRow, mask_ip, port_key_string,
    refusal, split_url,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// What a line became, with what the totals need and the row doesn't keep.
#[derive(Debug, PartialEq)]
pub enum Cleaned {
    Request {
        row: RequestRow,
        visitor: u64,
        pageview: bool,
        /// The query's parameter names.
        params: Vec<String>,
    },
    Connection {
        row: ConnectionRow,
        visitor: u64,
    },
    Waf {
        row: WafRow,
        visitor: u64,
    },
}

/// Looks up an address's country code ("" when unknown).
pub trait Locate {
    fn country(&self, ip: &str) -> String;
}

impl<F: Fn(&str) -> String> Locate for F {
    fn country(&self, ip: &str) -> String {
        self(ip)
    }
}

pub struct Cleaner<'a> {
    pub ctx: &'a TrafficContext,
    /// Whether Pyo has said which hosts and ports are services' yet (until
    /// then everything is kept).
    pub known: bool,
    pub salt: &'a str,
    pub locate: &'a dyn Locate,
    pub ua: &'a woothee::parser::Parser,
}

fn hash(parts: &[&str]) -> u64 {
    let d = Sha256::digest(parts.join("|").as_bytes());
    u64::from_be_bytes(d[..8].try_into().unwrap_or([0; 8]))
}

fn capped(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The UTC day of a time, for the salt.
pub fn day_of(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

impl Cleaner<'_> {
    /// The rule behind a refusal: behind a CDN, a connection that didn't
    /// come from it (`peer`) is the CDN check's ("cdn"); else the rule that
    /// matches the address (or its country), else what the proxy said.
    fn rule(&self, service_id: &str, why: String, ip: &str, peer: &str, country: &str) -> String {
        if why != "blocked" && why != "rate-limit" {
            return why;
        }
        let (Some(f), Ok(addr)) = (self.ctx.firewalls.get(service_id), ip.parse::<IpAddr>()) else {
            return why;
        };
        if why == "blocked"
            && let Ok(peer) = peer.parse::<IpAddr>()
            && !pyo_proto::firewall::via_cdn(f, peer)
        {
            return pyo_proto::firewall::BEHIND_RULE.to_owned();
        }
        let rule = if why == "blocked" {
            pyo_proto::firewall::blocking_rule(&[f], addr, Some(country).filter(|c| !c.is_empty()))
        } else {
            pyo_proto::firewall::rate_limiting_rule(&[f], addr)
        };
        rule.map(str::to_owned).unwrap_or(why)
    }

    pub fn clean(&self, l: &Value, now_ms: i64) -> Option<Cleaned> {
        // The WAF's finding for one request (pyo-waf's line).
        if l.get("waf").is_some() {
            let host = l["host"].as_str().unwrap_or("").to_lowercase();
            if self.known && !self.ctx.hosts.contains_key(&host) {
                return None;
            }
            let at = l["time"]
                .as_str()
                .and_then(pyo_proto::traffic::parse_time_ms)
                .unwrap_or(now_ms);
            let ip = l["client"].as_str().unwrap_or("");
            let country = self.locate.country(ip);
            let (path, url, _, _) = split_url(l["uri"].as_str().unwrap_or("/"));
            let row = WafRow {
                at,
                host: host.clone(),
                method: capped(l["method"].as_str().unwrap_or(""), 16),
                path,
                url,
                ip: mask_ip(&self.ctx.ip_mode, ip),
                country,
                blocked: l["blocked"].as_bool().unwrap_or(false),
                rules: l["rules"].as_array().into_iter().flatten().filter_map(Value::as_u64).take(50).collect(),
                messages: l["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(50)
                    .map(|m| capped(m, 300))
                    .collect(),
                severity: capped(l["severity"].as_str().unwrap_or(""), 16),
                server: capped(l["PyoNode"].as_str().unwrap_or(""), 253),
            };
            let visitor = hash(&[self.salt, ip, "waf", &host]);
            return Some(Cleaned::Waf { row, visitor });
        }
        let l = AccessLine::read(l);
        let at = l.ts_ms.unwrap_or(now_ms);
        let ip = l.visitor_ip().to_owned();
        // A connection to a public TCP or UDP port.
        if let Some((proto, key)) = l.port_key() {
            let key = port_key_string(proto, &key);
            let owner = self.ctx.ports.get(&key);
            if self.known && owner.is_none() {
                return None;
            }
            let (service_id, port) = owner.cloned().unwrap_or_default();
            let country = self.locate.country(&ip);
            let why = refusal("", &l.termination, 0);
            let row = ConnectionRow {
                at,
                key: key.clone(),
                service_id: service_id.clone(),
                port,
                protocol: proto.to_owned(),
                ip: mask_ip(&self.ctx.ip_mode, &ip),
                duration_ms: l.duration_ms,
                bytes_in: l.bytes_in,
                bytes_out: l.bytes,
                outcome: l.outcome().to_owned(),
                rule: self.rule(&service_id, why, &ip, "", &country),
                country,
                server: capped(&l.node, 253),
            };
            let visitor = hash(&[self.salt, &ip, &key]);
            return Some(Cleaned::Connection { row, visitor });
        }
        let host = l.host.split(':').next().unwrap_or("").to_lowercase();
        let service_id = match self.ctx.hosts.get(&host) {
            Some(s) => s.clone(),
            None if self.known => return None,
            None => String::new(),
        };
        if host.is_empty() {
            return None;
        }
        let ua = l.user_agent.as_str();
        let status = l.status;
        let method = if l.method.is_empty() { "GET".to_owned() } else { capped(&l.method, 16) };
        let (path, url, keys, _) = split_url(if l.path.is_empty() { "/" } else { l.path.as_str() });
        let parsed = self.ua.parse(ua);
        let crawler = parsed.as_ref().is_some_and(|r| r.category == "crawler");
        let device = match parsed.as_ref().map(|r| r.category) {
            Some("pc") => "Desktop",
            Some("smartphone") | Some("mobilephone") => "Mobile",
            Some("crawler") => "Bot",
            Some("appliance") => "Other",
            _ if ua.is_empty() => "Unknown",
            _ => "Other",
        };
        let browser = parsed
            .as_ref()
            .map(|r| r.name)
            .filter(|n| !n.is_empty() && *n != "UNKNOWN")
            .unwrap_or(if ua.starts_with("curl/") { "curl" } else { "Other" });
        // A page view: a GET answered with HTML (a 304 has no body, so a path without an extension).
        let ctype = l.content_type.as_str();
        let html = ctype.starts_with("text/html")
            || (status == 304 && ctype.is_empty() && !path.rsplit('/').next().unwrap_or("").contains('.'));
        let pageview = method == "GET" && (200..400).contains(&status) && html && !crawler;
        let referrer = Some(l.referer.as_str())
            .filter(|r| !r.is_empty())
            .and_then(|r| r.split("://").nth(1))
            .map(|r| r.split(['/', '?', ':']).next().unwrap_or("").to_lowercase());
        let referrer = match referrer.as_deref() {
            None | Some("") => "Direct".to_owned(),
            Some(r) if r == host => String::new(),
            Some(r) => capped(r.trim_start_matches("www."), 253),
        };
        let country = self.locate.country(&ip);
        let why = refusal(&l.details, "", status);
        let row = RequestRow {
            at,
            host: host.clone(),
            method,
            path,
            url,
            status: status.min(999) as u16,
            duration_ms: l.duration_ms,
            bytes: l.bytes,
            referrer,
            device: device.to_owned(),
            browser: capped(browser, 64),
            ip: mask_ip(&self.ctx.ip_mode, &ip),
            // As sent; capped like a header line.
            user_agent: capped(ua, 512),
            server: capped(&l.node, 253),
            blocked_by: self.rule(&service_id, why, &ip, &l.peer, &country),
            waf: false,
            country,
        };
        let visitor = hash(&[self.salt, &ip, ua, &host]);
        Some(Cleaned::Request { row, visitor, pageview, params: keys })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> TrafficContext {
        serde_json::from_value(json!({
            "hosts": { "shop.example.com": "svc_1" },
            "ports": { "shop-cache-tcp-6379": ["svc_2", 6379] },
            "firewalls": { "svc_1": { "rules": [{ "id": "no-de", "action": "block", "sources": ["DE"] }] } },
            "ipMode": "anonymized",
        }))
        .unwrap()
    }

    fn cleaner<'a>(ctx: &'a TrafficContext, ua: &'a woothee::parser::Parser, locate: &'a dyn Locate) -> Cleaner<'a> {
        Cleaner { ctx, known: true, salt: "s", locate, ua }
    }

    #[test]
    fn a_request_is_cleaned_before_it_is_kept() {
        let ctx = ctx();
        let ua = woothee::parser::Parser::new();
        let locate = |ip: &str| if ip == "203.0.113.9" { "ID".to_owned() } else { String::new() };
        let c = cleaner(&ctx, &ua, &locate);
        let line = json!({ "start_time": "2026-11-06T12:28:32.253Z", "host": "Shop.example.com:443", "method": "GET", "path": "/reset?token=abc&page=2",
            "status": 200, "content_type": "text/html", "peer": "10.0.2.2", "remote": "203.0.113.9", "duration_ms": 12, "bytes": 900,
            "referer": "https://www.news.example/a", "user_agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36", "PyoNode": "sg-1" });
        let Some(Cleaned::Request { row, visitor, pageview, params }) = c.clean(&line, 0) else {
            panic!("a request")
        };
        assert_eq!(row.host, "shop.example.com");
        assert_eq!(row.url, "/reset?token=[redacted]&page=2");
        assert_eq!(row.ip, "203.0.113.0", "as the IP setting keeps it");
        assert_eq!(row.country, "ID", "looked up from the whole address");
        assert_eq!(row.referrer, "news.example");
        assert_eq!((row.device.as_str(), row.browser.as_str()), ("Desktop", "Chrome"));
        assert!(pageview);
        assert_eq!(params, ["token", "page"]);
        assert_eq!(visitor, hash(&["s", "203.0.113.9", row.user_agent.as_str(), "shop.example.com"]));
        // Another host on the gateway (a scanner, another cluster's name): not kept.
        let mut other = line.clone();
        other["host"] = json!("203.0.113.1");
        assert_eq!(c.clean(&other, 0), None);
    }

    #[test]
    fn refusals_name_their_rule() {
        let ctx = ctx();
        let ua = woothee::parser::Parser::new();
        let locate = |_: &str| "DE".to_owned();
        let c = cleaner(&ctx, &ua, &locate);
        let line = json!({ "host": "shop.example.com", "method": "GET", "path": "/", "status": 403, "remote": "198.51.100.4",
            "details": "rbac_access_denied_matched_policy[DENY]" });
        let Some(Cleaned::Request { row, pageview, .. }) = c.clean(&line, 5) else { panic!() };
        assert_eq!((row.blocked_by.as_str(), row.at, pageview), ("no-de", 5, false));
    }

    #[test]
    fn connections_and_waf_findings() {
        let ctx = ctx();
        let ua = woothee::parser::Parser::new();
        let locate = |_: &str| String::new();
        let c = cleaner(&ctx, &ua, &locate);
        let tcp = json!({ "peer": "10.0.2.2", "remote": "203.0.113.9", "local_port": "16379", "bytes": "120", "bytes_in": 64,
            "duration_ms": 5012, "upstream": "tcproute/pyo-gateway/shop-cache-tcp-6379/rule/0", "flags": "-" });
        let Some(Cleaned::Connection { row, .. }) = c.clean(&tcp, 0) else { panic!() };
        assert_eq!((row.service_id.as_str(), row.port, row.outcome.as_str(), row.ip.as_str()), ("svc_2", 6379, "ok", "203.0.113.0"));
        let waf = json!({ "waf": true, "host": "shop.example.com", "time": "2026-11-06T12:28:32Z", "client": "203.0.113.9", "uri": "/login?password=x",
            "method": "POST", "blocked": true, "rules": [942100], "messages": ["SQL Injection"], "severity": "critical" });
        let Some(Cleaned::Waf { row, .. }) = c.clean(&waf, 0) else { panic!() };
        assert_eq!((row.url.as_str(), row.blocked, row.rules.as_slice()), ("/login?password=[redacted]", true, &[942100][..]));
        // Before Pyo said which hosts are services': kept.
        let unknown = Cleaner { known: false, ..c };
        assert!(unknown.clean(&json!({ "host": "new.example.com", "status": 200 }), 0).is_some());
    }
}
