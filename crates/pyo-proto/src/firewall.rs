//! The firewall: who may reach a service, set per service as an ordered
//! list of rules (pyo-api stores them; the agent turns them into the
//! gateway's policies and network policies).
//!
//! A rule matches a client by its address or country: "is in" or "is not
//! in" a list of addresses, ranges or countries, or any client. Allow and
//! block rules are checked top to bottom and the first one that matches
//! decides; a client no rule matches gets in. Rate limit rules apply after
//! that, to websites only: each one that matches a request counts it, per
//! client address and per server, and a request over any of them is
//! refused (429). A proxied port's own IP rule (`port_level`) is checked
//! before the service's rules and can only narrow them.
//!
//! Where it's enforced:
//! - websites (through the proxy): addresses and countries, and rate limits;
//! - TCP ports through the proxy: addresses (the proxy can't place a raw
//!   connection in a country, so a country is never "in" a list there);
//! - UDP ports and ports that bypass the gateway: addresses, by network policy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The most rules a service holds.
pub const MAX_RULES: usize = 500;
/// The most values one rule lists.
pub const MAX_SOURCES: usize = 50;
/// The most rate limits the proxy takes per website (Envoy Gateway's
/// limit): "any client" takes two (IPv4 and IPv6), "is in" one per value,
/// "is not in" one.
pub const MAX_RATE_LIMITS: usize = 16;

/// A rate limit rule's limit: `requests` per `per` ("second" or "minute")
/// from each client address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimit {
    pub requests: u32,
    pub per: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirewallRule {
    pub id: String,
    /// "allow", "block" or "rateLimit".
    pub action: String,
    /// "in" or "notIn" (a list of sources); "in" with no sources: any client.
    #[serde(default = "is_in")]
    pub operator: String,
    /// Address ranges ("203.0.113.0/24", "2001:db8::/32") or countries'
    /// two-letter codes ("DE"), not both; none: any client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// A rate limit rule's limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<RateLimit>,
}

fn is_in() -> String {
    "in".into()
}

pub fn is_country(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_uppercase())
}

impl FirewallRule {
    pub fn blocks(&self) -> bool {
        self.action == "block"
    }
    pub fn rate_limits(&self) -> bool {
        self.action == "rateLimit"
    }
    pub fn negated(&self) -> bool {
        self.operator == "notIn"
    }
    /// Matches every client.
    pub fn any(&self) -> bool {
        self.sources.is_empty()
    }
    pub fn countries(&self) -> impl Iterator<Item = &str> {
        self.sources
            .iter()
            .map(String::as_str)
            .filter(|s| is_country(s))
    }
    pub fn cidrs(&self) -> impl Iterator<Item = &str> {
        self.sources
            .iter()
            .map(String::as_str)
            .filter(|s| s.contains('/'))
    }
    pub fn has_countries(&self) -> bool {
        self.countries().next().is_some()
    }

    /// What the rule lists, as a term: none when it can't match here
    /// (countries where they don't count). Ignores the operator.
    fn listed(&self, geo: bool) -> Option<Term> {
        if self.any() {
            return Some(Term::ALL);
        }
        if self.has_countries() {
            return geo.then(|| Term {
                cidrs: None,
                countries: Some(self.countries().map(str::to_owned).collect()),
            });
        }
        Some(Term {
            cidrs: Some(self.cidrs().map(str::to_owned).collect()),
            countries: None,
        })
    }

    /// Whether it matches a client (`country` when known).
    fn matches(&self, ip: IpAddr, country: Option<&str>) -> bool {
        if self.any() {
            return true;
        }
        let host = Addrs::of_ip(ip);
        let listed = self.sources.iter().any(|s| {
            if is_country(s) {
                country == Some(s.as_str())
            } else {
                !Addrs::of(&[s.as_str()]).intersect(&host).is_empty()
            }
        });
        listed != self.negated()
    }
}

/// A service's firewall: its rules, in order (or a port's own IP rule,
/// `port_level`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Firewall {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<FirewallRule>,
}

impl Firewall {
    /// No rules: everyone gets in, without a rate limit.
    pub fn is_unrestricted(&self) -> bool {
        self.rules.is_empty()
    }

    /// Any country rule: the proxies then place visitors by their
    /// connection (`system::WebSettings::direct`).
    pub fn has_countries(&self) -> bool {
        self.rules.iter().any(FirewallRule::has_countries)
    }
}

/// A port's own IP rule (`PortProxy::ip_filter`) as a layer of its own:
/// one block rule, checked before the service's rules.
pub fn port_level(f: &crate::IpFilter) -> Firewall {
    let operator = if f.mode == "allow" { "notIn" } else { "in" };
    Firewall {
        rules: vec![FirewallRule {
            id: "port".into(),
            action: "block".into(),
            operator: operator.into(),
            sources: f.cidrs.clone(),
            limit: None,
        }],
    }
}

/// The allow and block rules of the layers, in the order they're checked.
fn access<'a>(layers: &[&'a Firewall]) -> impl Iterator<Item = &'a FirewallRule> {
    layers
        .iter()
        .flat_map(|l| l.rules.iter())
        .filter(|r| !r.rate_limits())
}

// ── Terms: what an authorization rule's principal can say ───────────────────

/// A set of sources: any address in `cidrs` (none: any address) that's also
/// in one of `countries` (none: any country).
#[derive(Debug, Clone, PartialEq)]
struct Term {
    cidrs: Option<Vec<String>>,
    countries: Option<Vec<String>>,
}

impl Term {
    const ALL: Term = Term {
        cidrs: None,
        countries: None,
    };

    fn principal(&self) -> Value {
        let mut p = serde_json::Map::new();
        p.insert(
            "clientCIDRs".into(),
            json!(
                self.cidrs
                    .clone()
                    .unwrap_or_else(|| vec!["0.0.0.0/0".into(), "::/0".into()])
            ),
        );
        if let Some(cc) = &self.countries {
            // Countries alone: any address in them.
            if self.cidrs.is_none() {
                p.remove("clientCIDRs");
            }
            p.insert(
                "clientIPGeoLocations".into(),
                json!(
                    cc.iter()
                        .map(|c| json!({ "country": c }))
                        .collect::<Vec<_>>()
                ),
            );
        }
        Value::Object(p)
    }
}

fn parse(c: &str) -> Option<(IpAddr, u8)> {
    let (a, b) = c.split_once('/')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Whether range `a` lies inside range `b`.
fn within(a: &str, b: &str) -> bool {
    let (Some((aa, ab)), Some((ba, bb))) = (parse(a), parse(b)) else {
        return false;
    };
    if ab < bb {
        return false;
    }
    match (aa, ba) {
        (IpAddr::V4(x), IpAddr::V4(y)) => {
            bb == 0 || (u32::from(x) >> (32 - bb as u32)) == (u32::from(y) >> (32 - bb as u32))
        }
        (IpAddr::V6(x), IpAddr::V6(y)) => {
            bb == 0 || (u128::from(x) >> (128 - bb as u32)) == (u128::from(y) >> (128 - bb as u32))
        }
        _ => false,
    }
}

/// Two ranges' overlap: the smaller one when one holds the other, else none
/// (ranges never partly overlap).
fn overlap(a: &str, b: &str) -> Option<String> {
    if within(a, b) {
        Some(a.to_owned())
    } else if within(b, a) {
        Some(b.to_owned())
    } else {
        None
    }
}

/// Both terms: none when nothing is in both.
fn and(x: &Term, y: &Term) -> Option<Term> {
    let cidrs = match (&x.cidrs, &y.cidrs) {
        (None, c) | (c, None) => c.clone(),
        (Some(a), Some(b)) => {
            let mut v: Vec<String> = a
                .iter()
                .flat_map(|p| b.iter().filter_map(move |q| overlap(p, q)))
                .collect();
            v.sort();
            v.dedup();
            if v.is_empty() {
                return None;
            }
            Some(v)
        }
    };
    let countries = match (&x.countries, &y.countries) {
        (None, c) | (c, None) => c.clone(),
        (Some(a), Some(b)) => {
            let v: Vec<String> = a.iter().filter(|c| b.contains(c)).cloned().collect();
            if v.is_empty() {
                return None;
            }
            Some(v)
        }
    };
    Some(Term { cidrs, countries })
}

/// Envoy Gateway's authorization for one route, from the layers' allow and
/// block rules (the first that matches decides, as Envoy Gateway checks its
/// rules): none when nothing is refused. `geo`: countries count
/// (websites); elsewhere a country is never "in" a list.
///
/// An "is not in" rule can't be said directly: the rules after it apply
/// only to what it lists (each is narrowed to that), what it lists and no
/// later rule matches gets the outcome it would have had without it, and
/// everything else gets its action, after those. Every rule that refuses
/// is named after its rule, so the access log's "denied" says which kind it
/// was (the API names the rule from the address, `blocking_rule`).
pub fn authorization(layers: &[&Firewall], geo: bool) -> Option<Value> {
    let mut rules = vec![];
    let push = |rules: &mut Vec<Value>, name: &str, action: &str, t: &Term| {
        let action = if action == "block" { "Deny" } else { "Allow" };
        rules.push(json!({ "name": name, "action": action, "principal": t.principal() }));
    };
    // What later rules still see (none: nothing), and the "is not in"
    // rules passed: (what the rules saw then, the rule).
    let mut rest = Some(Term::ALL);
    let mut negated: Vec<(Term, &FirewallRule)> = vec![];
    for r in access(layers) {
        let Some(seen) = rest.clone() else { break };
        if r.negated() {
            rest = r.listed(geo).and_then(|t| and(&seen, &t));
            negated.push((seen, r));
        } else if let Some(t) = r.listed(geo).and_then(|t| and(&seen, &t)) {
            push(&mut rules, &r.id, &r.action, &t);
        }
    }
    let default = match negated.split_first() {
        None => "Allow",
        Some(((_, first), inner)) => {
            if let Some(t) = &rest {
                push(&mut rules, "rest", "allow", t);
            }
            for (seen, r) in inner.iter().rev() {
                push(&mut rules, &r.id, &r.action, seen);
            }
            if first.blocks() { "Deny" } else { "Allow" }
        }
    };
    if default == "Allow" && rules.iter().all(|r| r["action"] == "Allow") {
        return None;
    }
    Some(json!({ "defaultAction": default, "rules": rules }))
}

// ── Address sets: what a network policy can say ─────────────────────────────

/// A set of addresses: sorted, disjoint inclusive ranges, IPv4 (`false`)
/// before IPv6 (`true`).
#[derive(Debug, Clone, Default, PartialEq)]
struct Addrs(Vec<(bool, u128, u128)>);

const fn top(v6: bool) -> u128 {
    if v6 { u128::MAX } else { u32::MAX as u128 }
}

impl Addrs {
    fn all() -> Self {
        Addrs(vec![(false, 0, top(false)), (true, 0, top(true))])
    }
    fn of_ip(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(a) => Addrs(vec![(false, u32::from(a) as u128, u32::from(a) as u128)]),
            IpAddr::V6(a) => Addrs(vec![(true, u128::from(a), u128::from(a))]),
        }
    }
    fn of(cidrs: &[&str]) -> Self {
        let mut v = vec![];
        for (a, bits) in cidrs.iter().filter_map(|c| parse(c)) {
            let (v6, n, width) = match a {
                IpAddr::V4(a) => (false, u32::from(a) as u128, 32u32),
                IpAddr::V6(a) => (true, u128::from(a), 128u32),
            };
            let host = width.saturating_sub(bits as u32);
            let mask = if host >= 128 {
                u128::MAX
            } else {
                (1u128 << host) - 1
            };
            v.push((v6, n & !mask, (n & !mask) | mask));
        }
        Self::normal(v)
    }
    fn normal(mut v: Vec<(bool, u128, u128)>) -> Self {
        v.sort();
        let mut out: Vec<(bool, u128, u128)> = vec![];
        for (f, a, b) in v {
            match out.last_mut() {
                Some((lf, _, lb)) if *lf == f && (*lb == u128::MAX || a <= *lb + 1) => {
                    *lb = (*lb).max(b)
                }
                _ => out.push((f, a, b)),
            }
        }
        Addrs(out)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn union(&self, o: &Addrs) -> Addrs {
        Self::normal(self.0.iter().chain(&o.0).copied().collect())
    }
    fn minus(&self, o: &Addrs) -> Addrs {
        let mut out = vec![];
        for &(f, a, b) in &self.0 {
            let mut parts = vec![(a, b)];
            for &(_, c, d) in o.0.iter().filter(|x| x.0 == f) {
                parts = parts
                    .into_iter()
                    .flat_map(|(x, y)| {
                        if d < x || c > y {
                            return vec![(x, y)];
                        }
                        let mut keep = vec![];
                        if c > x {
                            keep.push((x, c - 1));
                        }
                        if d < y {
                            keep.push((d + 1, y));
                        }
                        keep
                    })
                    .collect();
            }
            out.extend(parts.into_iter().map(|(x, y)| (f, x, y)));
        }
        Addrs(out)
    }
    fn intersect(&self, o: &Addrs) -> Addrs {
        self.minus(&self.minus(o))
    }
    fn family(&self, v6: bool) -> Addrs {
        Addrs(self.0.iter().filter(|x| x.0 == v6).copied().collect())
    }
    /// As few ranges as cover it exactly.
    fn cidrs(&self) -> Vec<String> {
        let mut out = vec![];
        for &(v6, a, b) in &self.0 {
            let width: u32 = if v6 { 128 } else { 32 };
            let mut start = a;
            loop {
                let span = b - start;
                let mut bits = if start == 0 {
                    width
                } else {
                    start.trailing_zeros().min(width)
                };
                while bits > 0
                    && !(if bits == 128 {
                        span == u128::MAX
                    } else {
                        (1u128 << bits) - 1 <= span
                    })
                {
                    bits -= 1;
                }
                let addr = if v6 {
                    IpAddr::V6(Ipv6Addr::from(start))
                } else {
                    IpAddr::V4(Ipv4Addr::from(start as u32))
                };
                out.push(format!("{addr}/{}", width - bits));
                if bits == 128 {
                    break;
                }
                let last = start + ((1u128 << bits) - 1);
                if last >= b {
                    break;
                }
                start = last + 1;
            }
        }
        out
    }
}

/// The addresses a rule matches, as a network policy sees them (no
/// countries: a country is never "in" a list).
fn addrs(r: &FirewallRule) -> Addrs {
    let listed = if r.any() {
        Addrs::all()
    } else {
        Addrs::of(&r.cidrs().collect::<Vec<_>>())
    };
    if r.negated() {
        Addrs::all().minus(&listed)
    } else {
        listed
    }
}

/// The address ranges a network policy lets reach a port: (range, except).
/// None when nothing is refused. Countries don't count here.
pub fn ip_blocks(layers: &[&Firewall]) -> Option<Vec<(String, Vec<String>)>> {
    let mut rest = Addrs::all();
    let mut allowed = Addrs::default();
    for r in access(layers) {
        let hit = addrs(r).intersect(&rest);
        if !r.blocks() {
            allowed = allowed.union(&hit);
        }
        rest = rest.minus(&hit);
    }
    let allowed = allowed.union(&rest);
    if allowed == Addrs::all() {
        return None;
    }
    // Per family: its ranges, or everything but what's refused, whichever
    // is shorter.
    let mut out = vec![];
    for (v6, all) in [(false, "0.0.0.0/0"), (true, "::/0")] {
        let mine = allowed.family(v6);
        if mine.is_empty() {
            continue;
        }
        let listed = mine.cidrs();
        let except = Addrs::all().family(v6).minus(&mine).cidrs();
        if except.len() + 1 < listed.len() {
            out.push((all.to_owned(), except));
        } else {
            out.extend(listed.into_iter().map(|c| (c, vec![])));
        }
    }
    Some(out)
}

// ── Rate limits ──────────────────────────────────────────────────────────────

/// How many of the proxy's rate limits a rule takes ([`MAX_RATE_LIMITS`]).
pub fn rate_limit_cost(r: &FirewallRule) -> usize {
    if r.any() {
        2
    } else if r.negated() {
        1
    } else {
        r.sources.len()
    }
}

/// A website's rate limits, as Envoy Gateway's local rate limit: each
/// client address counted on its own (`Distinct`), on each server. None
/// without rate limit rules. Countries can't be counted (the API refuses
/// them); "is not in" takes one range (an inverted match).
pub fn rate_limit(fw: &Firewall) -> Option<Value> {
    let mut rules = vec![];
    for r in fw.rules.iter().filter(|r| r.rate_limits()) {
        let Some(l) = &r.limit else { continue };
        let unit = if l.per == "second" {
            "Second"
        } else {
            "Minute"
        };
        let rule = |cidr: &str, invert: bool| {
            let mut sel = json!({ "type": "Distinct", "value": cidr });
            if invert {
                sel["invert"] = json!(true);
            }
            json!({ "clientSelectors": [{ "sourceCIDR": sel }], "limit": { "requests": l.requests, "unit": unit } })
        };
        if r.any() {
            rules.extend([rule("0.0.0.0/0", false), rule("::/0", false)]);
        } else if r.negated() {
            rules.extend(r.cidrs().take(1).map(|c| rule(c, true)));
        } else {
            rules.extend(r.cidrs().map(|c| rule(c, false)));
        }
    }
    rules.truncate(MAX_RATE_LIMITS);
    (!rules.is_empty()).then(|| json!({ "type": "Local", "local": { "rules": rules } }))
}

// ── Checking one client ─────────────────────────────────────────────────────

/// The first allow or block rule that matches a client, if any.
fn deciding<'a>(
    layers: &[&'a Firewall],
    ip: IpAddr,
    country: Option<&str>,
) -> Option<&'a FirewallRule> {
    access(layers).find(|r| r.matches(ip, country))
}

/// The block rule that turned an address away (the proxy logs only that
/// some rule refused it).
pub fn blocking_rule<'a>(
    layers: &[&'a Firewall],
    ip: IpAddr,
    country: Option<&str>,
) -> Option<&'a str> {
    deciding(layers, ip, country)
        .filter(|r| r.blocks())
        .map(|r| r.id.as_str())
}

/// The first rate limit rule that counts an address (the proxy logs only
/// that some limit refused it).
pub fn rate_limiting_rule<'a>(layers: &[&'a Firewall], ip: IpAddr) -> Option<&'a str> {
    layers
        .iter()
        .flat_map(|l| l.rules.iter())
        .find(|r| r.rate_limits() && r.matches(ip, None))
        .map(|r| r.id.as_str())
}

/// Whether an address gets through, as the policies above decide (the
/// dashboard's lockout check does the same). `country`: the address's
/// country when known.
pub fn admits(layers: &[&Firewall], ip: IpAddr, country: Option<&str>) -> bool {
    deciding(layers, ip, country).is_none_or(|r| !r.blocks())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, action: &str, operator: &str, sources: &[&str]) -> FirewallRule {
        FirewallRule {
            id: id.into(),
            action: action.into(),
            operator: operator.into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            limit: None,
        }
    }

    fn fw(rules: Vec<FirewallRule>) -> Firewall {
        Firewall { rules }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn nothing_refused_is_nothing() {
        assert_eq!(
            authorization(
                &[
                    &fw(vec![]),
                    &fw(vec![rule("a", "allow", "in", &["1.2.3.4/32"])])
                ],
                true
            ),
            None
        );
        assert_eq!(ip_blocks(&[&fw(vec![])]), None);
        assert_eq!(
            ip_blocks(&[&fw(vec![rule("a", "allow", "in", &["1.2.3.4/32"])])]),
            None
        );
        assert!(Firewall::default().is_unrestricted());
    }

    #[test]
    fn blocks_in_order_and_named() {
        let c = fw(vec![
            rule("fwr_c", "block", "in", &["203.0.113.0/24"]),
            rule("fwr_ru", "block", "in", &["RU"]),
            rule("fwr_s", "block", "in", &["198.51.100.7/32"]),
        ]);
        let a = authorization(&[&c], true).unwrap();
        assert_eq!(a["defaultAction"], "Allow");
        let names: Vec<&str> = a["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["fwr_c", "fwr_ru", "fwr_s"]);
        assert_eq!(
            a["rules"][1]["principal"],
            json!({ "clientIPGeoLocations": [{ "country": "RU" }] })
        );
        // Without countries (TCP): the country rule is left out.
        assert_eq!(
            authorization(&[&c], false).unwrap()["rules"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn first_match_decides() {
        // Allow the office, then block its network.
        let c = fw(vec![
            rule("ok", "allow", "in", &["10.1.2.3/32"]),
            rule("net", "block", "in", &["10.0.0.0/8"]),
        ]);
        let a = authorization(&[&c], true).unwrap();
        assert_eq!(a["rules"][0]["action"], "Allow");
        assert_eq!(a["rules"][1]["action"], "Deny");
        assert!(admits(&[&c], ip("10.1.2.3"), None));
        assert!(!admits(&[&c], ip("10.1.2.4"), None));
        assert_eq!(blocking_rule(&[&c], ip("10.1.2.4"), None), Some("net"));
        let blocks = ip_blocks(&[&c]).unwrap();
        let lets = |a: &str| {
            blocks
                .iter()
                .any(|(r, except)| within(a, r) && !except.iter().any(|e| within(a, e)))
        };
        assert!(
            lets("10.1.2.3/32")
                && !lets("10.1.2.4/32")
                && lets("11.0.0.1/32")
                && lets("2001:db8::1/128")
        );
    }

    #[test]
    fn not_in_narrows_the_rules_after_it() {
        // Only 10.0.0.0/8 and DE, but never 10.9.0.0/16.
        let c = fw(vec![
            rule("blk", "block", "in", &["10.9.0.0/16"]),
            rule("only", "block", "notIn", &["10.0.0.0/8"]),
            rule("de", "block", "notIn", &["DE"]),
        ]);
        let a = authorization(&[&c], true).unwrap();
        assert_eq!(a["defaultAction"], "Deny");
        assert_eq!(
            a["rules"],
            json!([
                { "name": "blk", "action": "Deny", "principal": { "clientCIDRs": ["10.9.0.0/16"] } },
                { "name": "rest", "action": "Allow", "principal": { "clientCIDRs": ["10.0.0.0/8"], "clientIPGeoLocations": [{ "country": "DE" }] } },
                { "name": "de", "action": "Deny", "principal": { "clientCIDRs": ["10.0.0.0/8"] } },
            ])
        );
        assert!(admits(&[&c], ip("10.1.1.1"), Some("DE")));
        assert!(!admits(&[&c], ip("10.1.1.1"), Some("FR")));
        assert_eq!(blocking_rule(&[&c], ip("10.1.1.1"), Some("FR")), Some("de"));
        assert_eq!(
            blocking_rule(&[&c], ip("11.1.1.1"), Some("DE")),
            Some("only")
        );
        assert!(!admits(&[&c], ip("10.9.1.1"), Some("DE")));
        assert!(!admits(&[&c], ip("2001:db8::1"), Some("DE")));
        // TCP: no country is in a list, so no one gets in.
        let a = authorization(&[&c], false).unwrap();
        assert_eq!(a["defaultAction"], "Deny");
        assert!(
            a["rules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["action"] == "Deny")
        );
        assert_eq!(ip_blocks(&[&c]), Some(vec![]));
    }

    #[test]
    fn allow_list_then_block_everyone() {
        let c = fw(vec![
            rule("a", "allow", "in", &["203.0.113.0/24", "2001:db8::/32"]),
            rule("b", "block", "in", &[]),
        ]);
        let a = authorization(&[&c], true).unwrap();
        assert_eq!(
            a["rules"][1]["principal"],
            json!({ "clientCIDRs": ["0.0.0.0/0", "::/0"] })
        );
        assert!(admits(&[&c], ip("203.0.113.9"), None) && !admits(&[&c], ip("198.51.100.1"), None));
        assert_eq!(
            ip_blocks(&[&c]),
            Some(vec![
                ("203.0.113.0/24".into(), vec![]),
                ("2001:db8::/32".into(), vec![])
            ])
        );
    }

    #[test]
    fn a_ports_rule_only_narrows_the_service() {
        // The port: only 10.1.0.0/16 and 192.168.0.0/16. The service: allows 10.1.2.3, blocks 10.0.0.0/8.
        let port = port_level(&crate::IpFilter {
            mode: "allow".into(),
            cidrs: vec!["10.1.0.0/16".into(), "192.168.0.0/16".into()],
        });
        let c = fw(vec![
            rule("ok", "allow", "in", &["10.1.2.3/32"]),
            rule("net", "block", "in", &["10.0.0.0/8"]),
        ]);
        assert!(admits(&[&port, &c], ip("10.1.2.3"), None));
        assert!(!admits(&[&port, &c], ip("10.1.2.4"), None));
        assert!(!admits(&[&port, &c], ip("172.16.0.1"), None));
        assert!(admits(&[&port, &c], ip("192.168.1.1"), None));
        let a = authorization(&[&port, &c], false).unwrap();
        assert_eq!(a["defaultAction"], "Deny");
        assert_eq!(
            a["rules"][0],
            json!({ "name": "ok", "action": "Allow", "principal": { "clientCIDRs": ["10.1.2.3/32"] } })
        );
        assert_eq!(
            ip_blocks(&[&port, &c]),
            Some(vec![
                ("10.1.2.3/32".into(), vec![]),
                ("192.168.0.0/16".into(), vec![])
            ])
        );
        // A port's block list.
        let port = port_level(&crate::IpFilter {
            mode: "deny".into(),
            cidrs: vec!["10.1.2.3/32".into()],
        });
        assert!(!admits(&[&port, &c], ip("10.1.2.3"), None));
    }

    #[test]
    fn rate_limits() {
        let mut r = rule("r", "rateLimit", "in", &[]);
        r.limit = Some(RateLimit {
            requests: 100,
            per: "minute".into(),
        });
        let mut s = rule("s", "rateLimit", "notIn", &["10.0.0.0/8"]);
        s.limit = Some(RateLimit {
            requests: 5,
            per: "second".into(),
        });
        let c = fw(vec![r.clone(), s.clone()]);
        let l = rate_limit(&c).unwrap();
        let rules = l["local"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(
            rules[0],
            json!({ "clientSelectors": [{ "sourceCIDR": { "type": "Distinct", "value": "0.0.0.0/0" } }], "limit": { "requests": 100, "unit": "Minute" } })
        );
        assert_eq!(
            rules[2]["clientSelectors"][0]["sourceCIDR"],
            json!({ "type": "Distinct", "value": "10.0.0.0/8", "invert": true })
        );
        assert_eq!(rules[2]["limit"]["unit"], "Second");
        // Rate limits don't refuse anyone on their own.
        assert_eq!(authorization(&[&c], true), None);
        assert_eq!(ip_blocks(&[&c]), None);
        assert!(!c.is_unrestricted());
        assert_eq!(rate_limiting_rule(&[&fw(vec![s])], ip("10.1.1.1")), None);
        assert_eq!(rate_limiting_rule(&[&c], ip("10.1.1.1")), Some("r"));
        assert_eq!(rate_limit(&fw(vec![rule("x", "block", "in", &[])])), None);
    }

    #[test]
    fn address_sets() {
        assert_eq!(Addrs::all().cidrs(), ["0.0.0.0/0", "::/0"]);
        let a = Addrs::all()
            .family(false)
            .minus(&Addrs::of(&["203.0.113.0/24"]));
        assert_eq!(a.cidrs().len(), 24);
        assert_eq!(
            a.union(&Addrs::of(&["203.0.113.0/24"])),
            Addrs::all().family(false)
        );
        assert_eq!(
            Addrs::of(&["10.0.0.0/8", "10.1.0.0/16"]).cidrs(),
            ["10.0.0.0/8"]
        );
        assert_eq!(
            Addrs::of(&["10.0.0.0/25", "10.0.0.128/25"]).cidrs(),
            ["10.0.0.0/24"]
        );
        assert!(
            Addrs::of(&["10.0.0.0/8"])
                .intersect(&Addrs::of_ip(ip("2001:db8::1")))
                .is_empty()
        );
    }
}
