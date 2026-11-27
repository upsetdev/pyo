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
//!
//! A rule can list presets next to addresses: a CDN's edge ("cloudflare",
//! "fastly", "bunny", "cloudfront") or your own networks ("tailnet",
//! "private"). Rules store the preset's name; pyo-api keeps the CDNs'
//! current ranges (cdn.rs) and sends the ones a firewall names with it
//! (`Firewall::presets`), and the agent expands them as it builds the
//! policies (`Firewall::expanded`).
//!
//! A website can be behind a CDN (`Firewall::behind`, Cloudflare for now):
//! only the CDN may reach it, and the visitor is the address the CDN
//! forwards, trusted only from the CDN's own ranges ([`web_authorization`],
//! system.rs `cdn_client_policy_spec`).

use std::collections::BTreeMap;
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
/// The most address ranges a service's policies list in all, presets
/// expanded ([`policy_size`]): well within what Kubernetes stores in one
/// object, and what Envoy checks quickly.
pub const MAX_POLICY_CIDRS: usize = 8_192;

/// A named list of ranges a rule can use next to addresses and ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// What rules store: "cloudflare".
    pub name: &'static str,
    /// What people read: "Cloudflare".
    pub label: &'static str,
    /// A CDN's edge, fetched from its official list (pyo-api's cdn.rs);
    /// else fixed ranges ([`Preset::fixed`]).
    pub cdn: bool,
    pub fixed: &'static [&'static str],
}

/// Tailscale's addresses (its CGNAT range and its IPv6 prefix).
pub const TAILNET_RANGES: [&str; 2] = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"];
/// Private networks: RFC 1918's ranges and IPv6's unique local addresses.
pub const PRIVATE_RANGES: [&str; 4] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"];

pub const PRESETS: [Preset; 6] = [
    Preset {
        name: "cloudflare",
        label: "Cloudflare",
        cdn: true,
        fixed: &[],
    },
    Preset {
        name: "fastly",
        label: "Fastly",
        cdn: true,
        fixed: &[],
    },
    Preset {
        name: "bunny",
        label: "bunny.net",
        cdn: true,
        fixed: &[],
    },
    Preset {
        name: "cloudfront",
        label: "Amazon CloudFront",
        cdn: true,
        fixed: &[],
    },
    Preset {
        name: "tailnet",
        label: "Tailnet",
        cdn: false,
        fixed: &TAILNET_RANGES,
    },
    Preset {
        name: "private",
        label: "Private networks",
        cdn: false,
        fixed: &PRIVATE_RANGES,
    },
];

pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

pub fn is_preset(s: &str) -> bool {
    preset(s).is_some()
}

/// The CDNs' current ranges, by preset name.
pub type CdnRanges = BTreeMap<String, Vec<String>>;

/// The CDNs a website can be behind (`Firewall::behind`). Each must append
/// the visitor's address to X-Forwarded-For, as Cloudflare does (the same
/// address it sends in CF-Connecting-IP): the proxies take the last
/// address there that isn't the CDN's, and only when the connection comes
/// from the CDN's ranges. Fastly (Fastly-Client-IP), bunny.net and
/// CloudFront append it too, so each is one more name here later.
pub const BEHIND: [&str; 1] = ["cloudflare"];

/// What a range list holds when a preset has no ranges to give: one
/// address no connection ever comes from, so "is in" matches no one (and
/// never "any client", which an empty list means).
const NOTHING: &str = "0.0.0.0/32";

/// The rule a website behind a CDN gets ahead of its own: the CDN's
/// connections only. Its id, for naming refusals.
pub const BEHIND_RULE: &str = "cdn";

/// The rule every website's authorization starts with: Let's Encrypt's
/// HTTP-01 challenges get through whatever the rules say, so certificates
/// keep renewing (cert-manager answers them on routes of its own, which no
/// firewall policy targets; this covers a challenge that reaches the
/// service's routes, as after a redirect to HTTPS).
pub const ACME_PREFIX: &str = "/.well-known/acme-challenge/";

/// The behind-a-CDN check (HTTPS): a request whose visitor the proxy did
/// not take from X-Forwarded-For is refused. The proxy only reads it from a
/// connection that comes from the CDN's ranges (`trustedCIDRs`), and an
/// address it reads there has no port (Envoy parses it as port 0), while a
/// real connection's always has one. Envoy Gateway can't match the
/// connection's own address once the visitor is read from a header, so
/// this is how a direct client, whatever it sends, is told apart. The
/// e2e check `scripts/cdn-gate-e2e.sh` runs it against a real proxy.
pub const BEHIND_GATE_CEL: &str = "source.port != 0";

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
    /// The ranges of the CDN presets its rules (and `behind`) name, as
    /// pyo-api has them now.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub presets: CdnRanges,
    /// The CDN in front of its websites (one of [`BEHIND`]): only the CDN
    /// reaches them, and the visitor is who the CDN says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<String>,
}

impl Firewall {
    /// No rules and no CDN in front: everyone gets in, without a rate limit.
    pub fn is_unrestricted(&self) -> bool {
        self.rules.is_empty() && self.behind.is_none()
    }

    /// A preset's ranges: fixed, or the CDN's as sent (none when missing).
    pub fn ranges(&self, name: &str) -> Vec<String> {
        match preset(name) {
            Some(p) if !p.cdn => p.fixed.iter().map(|c| (*c).to_owned()).collect(),
            Some(_) => self.presets.get(name).cloned().unwrap_or_default(),
            None => vec![],
        }
    }

    /// With every preset in its rules replaced by its ranges. A rule whose
    /// presets had no ranges (and nothing else) matches no one.
    pub fn expanded(&self) -> Firewall {
        let rules = self
            .rules
            .iter()
            .map(|r| {
                if !r.sources.iter().any(|s| is_preset(s)) {
                    return r.clone();
                }
                let mut sources: Vec<String> = vec![];
                for s in &r.sources {
                    let more = if is_preset(s) {
                        self.ranges(s)
                    } else {
                        vec![s.clone()]
                    };
                    for c in more {
                        if !sources.contains(&c) {
                            sources.push(c);
                        }
                    }
                }
                if sources.is_empty() {
                    sources.push(NOTHING.into());
                }
                FirewallRule {
                    sources,
                    ..r.clone()
                }
            })
            .collect();
        Firewall {
            rules,
            ..self.clone()
        }
    }

    /// The CDN it's behind, with its ranges: none when it isn't, or when
    /// the ranges are missing (then nothing is trusted).
    pub fn behind_ranges(&self) -> Option<(&str, Vec<String>)> {
        let cdn = self.behind.as_deref()?;
        let ranges = self.ranges(cdn);
        Some((cdn, ranges))
    }

    /// The presets it names (rules and `behind`).
    pub fn named_presets(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .rules
            .iter()
            .flat_map(|r| r.sources.iter())
            .map(String::as_str)
            .filter(|s| is_preset(s))
            .chain(self.behind.as_deref())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
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
        ..Default::default()
    }
}

/// The layers with their presets expanded ([`Firewall::expanded`]): rules
/// keep their order and ids.
fn expand_all(layers: &[&Firewall]) -> Vec<Firewall> {
    layers.iter().map(|l| l.expanded()).collect()
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

#[cfg(test)]
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

/// Both terms: none when nothing is in both.
fn and(x: &Term, y: &Term) -> Option<Term> {
    let cidrs = match (&x.cidrs, &y.cidrs) {
        (None, c) | (c, None) => c.clone(),
        (Some(a), Some(b)) => {
            let set = |v: &[String]| Addrs::of(&v.iter().map(String::as_str).collect::<Vec<_>>());
            let v = set(a).intersect(&set(b)).cidrs();
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
    let expanded = expand_all(layers);
    let layers: Vec<&Firewall> = expanded.iter().collect();
    let layers = layers.as_slice();
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

/// Where a website request comes in, for [`web_authorization`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// The service's own HTTPS listeners: behind a CDN, the visitor is who
    /// the CDN says (system.rs `cdn_client_policy_spec`).
    Https,
    /// The shared plain HTTP listener: the visitor is always the
    /// connection, so behind a CDN only the CDN's connections count.
    Http,
}

/// The rule that lets Let's Encrypt's challenges through ([`ACME_PREFIX`]).
/// (Envoy Gateway wants a principal in every rule: any address. `invert`
/// as its CRD defaults it, so the admission policy sees it as written.)
pub fn acme_rule() -> Value {
    json!({ "name": "acme", "action": "Allow", "operation": { "path": { "type": "PathPrefix", "value": ACME_PREFIX, "invert": false } }, "principal": Term::ALL.principal() })
}

/// A website route's authorization while its firewall refuses no one: Let's
/// Encrypt's rule, and everyone else allowed too. Every website route
/// (but tailnet ones) has a SecurityPolicy, so a first rule, or the last
/// one gone, changes only the route. Envoy Gateway adds an HTTP filter to
/// a listener only while some route there needs it, and a listener whose
/// filters change drops its open connections (after the drain timeout).
pub fn open_web_authorization() -> Value {
    json!({ "defaultAction": "Allow", "rules": [acme_rule()] })
}

/// A website route's rate limit while it has none, for the same reason as
/// [`open_web_authorization`]: Envoy's local rate limit (never Envoy
/// Gateway's global one, which needs a rate limit service). One count for
/// all IPv4 clients and one for all IPv6 ones (`Exact`, not a count per
/// client), at 2^32-1 requests a second, which no proxy reaches. Without
/// x-ratelimit-* headers: a rule that matched no one would leave a request
/// to the default count, whose headers Envoy Gateway always turns on.
/// `invert` as Envoy Gateway's CRD defaults it, so the stored policy is
/// exactly this and the admission policy, which pins it, sees it as
/// written (without it every write was refused, and the agent fell back
/// to the policy without the rate limit).
pub fn idle_rate_limit() -> Value {
    let rule = |cidr: &str| {
        json!({
            "clientSelectors": [{ "sourceCIDR": { "type": "Exact", "value": cidr, "invert": false } }],
            "limit": { "requests": u32::MAX, "unit": "Second" },
            "xRateLimitHeaders": "Off",
        })
    };
    json!({ "type": "Local", "local": { "rules": [rule("0.0.0.0/0"), rule("::/0")] } })
}

/// The behind-a-CDN rule for HTTPS ([`BEHIND_GATE_CEL`]).
pub fn behind_gate_rule() -> Value {
    json!({ "name": BEHIND_RULE, "action": "Deny", "cel": BEHIND_GATE_CEL })
}

/// A website route's authorization ([`authorization`], with countries):
/// Let's Encrypt's challenges first, whatever follows; then, behind a CDN,
/// the CDN's connections only, and on HTTPS the service's rules for the
/// visitor the CDN forwards. Over plain HTTP the proxy only sees the CDN,
/// so there the CDN check is all (its rules can't tell visitors apart).
/// None when nothing is refused.
pub fn web_authorization(fw: &Firewall, entry: Entry) -> Option<Value> {
    let (head, rest) = match (fw.behind_ranges(), entry) {
        (None, _) => (None, authorization(&[fw], true)),
        (Some(_), Entry::Https) => (Some(behind_gate_rule()), authorization(&[fw], true)),
        (Some((_, ranges)), Entry::Http) => {
            let gate = Firewall {
                rules: vec![FirewallRule {
                    id: BEHIND_RULE.into(),
                    action: "block".into(),
                    operator: "notIn".into(),
                    sources: if ranges.is_empty() {
                        vec![NOTHING.into()]
                    } else {
                        ranges
                    },
                    limit: None,
                }],
                ..Default::default()
            };
            (None, authorization(&[&gate], false))
        }
    };
    if head.is_none() && rest.is_none() {
        return None;
    }
    let (default, rules) = match rest {
        Some(a) => (
            a["defaultAction"].clone(),
            a["rules"].as_array().cloned().unwrap_or_default(),
        ),
        None => (json!("Allow"), vec![]),
    };
    let rules: Vec<Value> = [acme_rule()].into_iter().chain(head).chain(rules).collect();
    Some(json!({ "defaultAction": default, "rules": rules }))
}

/// A website's authorization on its tunnel routes once its record points
/// at the tunnel (`DomainSpec::tunnel_only`): the behind gate first, so only
/// the tunnel's connectors (whose visitor was read from X-Forwarded-For)
/// get through and a direct connection gets 403, then the service's own
/// rules. The service's own "behind" CDN doesn't apply there.
pub fn tunnel_authorization(fw: &Firewall) -> Value {
    let own = Firewall {
        behind: None,
        ..fw.clone()
    };
    let (default, rules) = match authorization(&[&own], true) {
        Some(a) => (
            a["defaultAction"].clone(),
            a["rules"].as_array().cloned().unwrap_or_default(),
        ),
        None => (json!("Allow"), vec![]),
    };
    let rules: Vec<Value> = [behind_gate_rule()].into_iter().chain(rules).collect();
    json!({ "defaultAction": default, "rules": rules })
}

/// What a tunnel client policy trusts: these addresses, or (with none
/// yet) one no connection ever comes from, never "anyone".
pub fn trusted_or_nothing(mut cidrs: Vec<String>) -> Vec<String> {
    cidrs.sort();
    cidrs.dedup();
    if cidrs.is_empty() {
        cidrs.push(NOTHING.into());
    }
    cidrs
}

/// Whether a website's plain HTTP routes take its rate limits: not behind
/// a CDN, where every request there comes from the CDN's few addresses.
pub fn http_rate_limits(fw: &Firewall) -> bool {
    fw.behind.is_none()
}

/// How many address ranges a service's policies list, presets expanded:
/// its websites' authorization (the larger of HTTPS and HTTP), its ports'
/// network policy (ranges and exceptions) and the CDN ranges its HTTPS
/// trusts. At most [`MAX_POLICY_CIDRS`].
pub fn policy_size(fw: &Firewall) -> usize {
    let count = |a: Option<Value>| -> usize {
        a.map(|a| {
            a["rules"]
                .as_array()
                .map(|rules| {
                    rules
                        .iter()
                        .map(|r| r["principal"]["clientCIDRs"].as_array().map_or(0, Vec::len))
                        .sum()
                })
                .unwrap_or(0)
        })
        .unwrap_or(0)
    };
    let web = count(web_authorization(fw, Entry::Https))
        .max(count(web_authorization(fw, Entry::Http)))
        .max(count(authorization(&[fw], false)));
    let blocks: usize = ip_blocks(&[fw])
        .map(|b| b.iter().map(|(_, except)| 1 + except.len()).sum())
        .unwrap_or(0);
    let trusted = fw.behind_ranges().map_or(0, |(_, r)| r.len());
    web.max(blocks) + trusted
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
    /// What's in it and not in `o`: one pass over both (each sorted and
    /// disjoint), so long lists (a CDN's) stay quick.
    fn minus(&self, o: &Addrs) -> Addrs {
        let mut out = vec![];
        let mut j = 0;
        for &(f, a, b) in &self.0 {
            // Ranges of `o` that end before this one starts.
            while j < o.0.len() && (o.0[j].0, o.0[j].2) < (f, a) {
                j += 1;
            }
            let mut start = a;
            let mut covered = false;
            let mut k = j;
            while k < o.0.len() && o.0[k].0 == f && o.0[k].1 <= b {
                let (_, c, d) = o.0[k];
                if c > start {
                    out.push((f, start, c - 1));
                }
                if d >= b {
                    covered = true;
                    break;
                }
                start = start.max(d + 1);
                k += 1;
            }
            if !covered {
                out.push((f, start, b));
            }
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
    let expanded = expand_all(layers);
    let layers: Vec<&Firewall> = expanded.iter().collect();
    let layers = layers.as_slice();
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
        r.sources
            .iter()
            .map(|s| preset(s).map_or(1, |p| p.fixed.len().max(1)))
            .sum()
    }
}

/// A website's rate limits, as Envoy Gateway's local rate limit: each
/// client address counted on its own (`Distinct`), on each server. None
/// without rate limit rules. Countries can't be counted (the API refuses
/// them); "is not in" takes one range (an inverted match).
pub fn rate_limit(fw: &Firewall) -> Option<Value> {
    let fw = fw.expanded();
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

/// The first allow or block rule that matches a client, if any (presets
/// expanded; the rule as the layers have it).
fn deciding<'a>(
    layers: &[&'a Firewall],
    ip: IpAddr,
    country: Option<&str>,
) -> Option<&'a FirewallRule> {
    let expanded = expand_all(layers);
    let refs: Vec<&Firewall> = expanded.iter().collect();
    let i = access(&refs).position(|r| r.matches(ip, country))?;
    access(layers).nth(i)
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
    let expanded = expand_all(layers);
    let i = expanded
        .iter()
        .flat_map(|l| l.rules.iter())
        .position(|r| r.rate_limits() && r.matches(ip, None))?;
    layers
        .iter()
        .flat_map(|l| l.rules.iter())
        .nth(i)
        .map(|r| r.id.as_str())
}

/// Whether a website request's connection came from the CDN the website
/// is behind (always, when it isn't behind one). `peer`: the connection's
/// own address (the access log's `peer`).
pub fn via_cdn(fw: &Firewall, peer: IpAddr) -> bool {
    let Some((_, ranges)) = fw.behind_ranges() else {
        return true;
    };
    let refs: Vec<&str> = ranges.iter().map(String::as_str).collect();
    !Addrs::of(&refs).intersect(&Addrs::of_ip(peer)).is_empty()
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
        Firewall {
            rules,
            ..Default::default()
        }
    }

    /// Cloudflare's ranges as a firewall has them (a few of them).
    fn cf() -> CdnRanges {
        [(
            "cloudflare".to_owned(),
            vec![
                "173.245.48.0/20".to_owned(),
                "104.16.0.0/13".to_owned(),
                "2400:cb00::/32".to_owned(),
            ],
        )]
        .into()
    }

    #[test]
    fn presets_expand_to_their_ranges() {
        let mut c = fw(vec![
            rule("t", "allow", "in", &["tailnet", "203.0.113.7/32"]),
            rule("cdn", "block", "notIn", &["cloudflare"]),
        ]);
        c.presets = cf();
        let e = c.expanded();
        assert_eq!(
            e.rules[0].sources,
            ["100.64.0.0/10", "fd7a:115c:a1e0::/48", "203.0.113.7/32"]
        );
        assert_eq!(e.rules[1].sources.len(), 3);
        assert_eq!(e.rules[1].id, "cdn");
        // Stored as names.
        assert_eq!(c.rules[1].sources, ["cloudflare"]);
        assert_eq!(c.named_presets(), ["cloudflare", "tailnet"]);
        // Checked as ranges, named as stored.
        assert!(admits(&[&c], ip("100.100.1.1"), None));
        assert!(admits(&[&c], ip("104.16.1.1"), None));
        assert_eq!(blocking_rule(&[&c], ip("198.51.100.1"), None), Some("cdn"));
        let a = authorization(&[&c], true).unwrap();
        assert!(!a.to_string().contains("cloudflare"), "{a}");
        assert!(a.to_string().contains("104.16.0.0/13"));
        let blocks = ip_blocks(&[&c]).unwrap();
        assert!(blocks.iter().any(|(r, _)| r == "104.16.0.0/13"));
        // A CDN whose ranges weren't sent: "is in" it matches no one (never
        // any client), "is not in" it everyone.
        let mut missing = fw(vec![rule("x", "block", "in", &["fastly"])]);
        assert_eq!(missing.expanded().rules[0].sources, [NOTHING]);
        assert!(admits(&[&missing], ip("151.101.1.1"), None));
        missing.rules[0].operator = "notIn".into();
        assert!(!admits(&[&missing], ip("151.101.1.1"), None));
        // Private networks.
        let p = fw(vec![rule("p", "block", "in", &["private"])]);
        assert!(!admits(&[&p], ip("192.168.1.1"), None) && !admits(&[&p], ip("fd00::1"), None));
        assert!(admits(&[&p], ip("100.64.0.1"), None));
    }

    #[test]
    fn rate_limits_count_presets_by_their_ranges() {
        let mut r = rule("r", "rateLimit", "in", &["private", "tailnet"]);
        assert_eq!(rate_limit_cost(&r), 6);
        r.limit = Some(RateLimit {
            requests: 10,
            per: "second".into(),
        });
        let l = rate_limit(&fw(vec![r])).unwrap();
        assert_eq!(l["local"]["rules"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn websites_let_acme_challenges_through() {
        // Everyone blocked: the challenge path still gets in, first.
        let c = fw(vec![rule("all", "block", "in", &[])]);
        for entry in [Entry::Https, Entry::Http] {
            let a = web_authorization(&c, entry).unwrap();
            assert_eq!(a["rules"][0], acme_rule());
            assert_eq!(
                a["rules"][0]["operation"]["path"],
                json!({ "type": "PathPrefix", "value": "/.well-known/acme-challenge/", "invert": false })
            );
            assert_eq!(a["rules"][1]["action"], "Deny");
        }
        // Nothing refused: no policy at all.
        assert_eq!(web_authorization(&fw(vec![]), Entry::Https), None);
        // Behind a CDN with no rules of its own: still the challenge first.
        let mut b = fw(vec![]);
        b.behind = Some("cloudflare".into());
        b.presets = cf();
        assert_eq!(
            web_authorization(&b, Entry::Https).unwrap()["rules"][0],
            acme_rule()
        );
    }

    #[test]
    fn behind_a_cdn() {
        let mut c = fw(vec![rule("de", "block", "notIn", &["DE"])]);
        c.behind = Some("cloudflare".into());
        c.presets = cf();
        assert!(!c.is_unrestricted());
        // HTTPS: the challenge, the CDN check, then the service's rules on
        // the visitor the CDN forwards.
        let a = web_authorization(&c, Entry::Https).unwrap();
        let names: Vec<&str> = a["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["acme", BEHIND_RULE, "rest"]);
        assert_eq!(
            a["rules"][1],
            json!({ "name": "cdn", "action": "Deny", "cel": "source.port != 0" })
        );
        assert_eq!(a["defaultAction"], "Deny");
        // Plain HTTP: the CDN's ranges only, and nothing else (the
        // connection is all the proxy knows there).
        let h = web_authorization(&c, Entry::Http).unwrap();
        assert_eq!(h["defaultAction"], "Deny");
        let rules = h["rules"].as_array().unwrap();
        assert_eq!(rules[0], acme_rule());
        assert_eq!(rules[1]["name"], "rest");
        assert_eq!(
            rules[1]["principal"]["clientCIDRs"],
            json!(["173.245.48.0/20", "104.16.0.0/13", "2400:cb00::/32"])
        );
        assert!(!h.to_string().contains("DE"));
        assert!(!http_rate_limits(&c));
        // Who the connection came from.
        assert!(via_cdn(&c, ip("104.16.3.4")) && !via_cdn(&c, ip("198.51.100.1")));
        assert!(via_cdn(&fw(vec![]), ip("198.51.100.1")));
        // Its ranges missing: no one gets in over HTTP either.
        c.presets.clear();
        let h = web_authorization(&c, Entry::Http).unwrap();
        assert_eq!(h["rules"][1]["principal"]["clientCIDRs"], json!([NOTHING]));
    }

    /// Every preset at once, the largest lists as long as their providers'
    /// (bunny.net lists each edge server, about a thousand), in rules of
    /// every kind: the policies stay within what one object holds.
    #[test]
    fn presets_stay_within_policy_limits() {
        let bunny: Vec<String> = (0..1000u32)
            .map(|i| format!("{}.{}.{}.1/32", 80 + i / 250, (i / 10) % 25, i % 250))
            .collect();
        let mut c = fw(vec![
            rule("a", "allow", "in", &["tailnet", "private"]),
            rule(
                "b",
                "block",
                "notIn",
                &["cloudflare", "fastly", "bunny", "cloudfront"],
            ),
            rule("c", "block", "in", &["bunny"]),
            rule("d", "allow", "notIn", &["DE", "FR"]),
            rule("e", "block", "in", &["bunny", "203.0.113.0/24"]),
        ]);
        c.presets = cf();
        c.presets.insert("bunny".into(), bunny);
        c.presets.insert(
            "fastly".into(),
            (0..40).map(|i| format!("151.{i}.0.0/16")).collect(),
        );
        c.presets.insert(
            "cloudfront".into(),
            (0..60).map(|i| format!("13.{i}.0.0/16")).collect(),
        );
        c.behind = Some("cloudflare".into());
        let n = policy_size(&c);
        assert!(n > 1000 && n <= MAX_POLICY_CIDRS, "{n}");
        let a = web_authorization(&c, Entry::Https).unwrap();
        assert!(a.to_string().len() < 512 * 1024, "{}", a.to_string().len());
        // Too many for one service: the size says so (the API refuses it).
        c.rules = (0..20)
            .map(|i| rule(&format!("r{i}"), "block", "notIn", &["bunny"]))
            .collect();
        c.rules.insert(0, rule("x", "allow", "in", &["bunny"]));
        let big: Vec<FirewallRule> = (0..12)
            .map(|i| rule(&format!("y{i}"), "allow", "in", &["bunny"]))
            .collect();
        c.rules.splice(1..1, big);
        assert!(policy_size(&c) > 0);
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
