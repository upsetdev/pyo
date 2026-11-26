//! Website and port traffic, as each cluster keeps it and Pyo stores it.
//!
//! The gateway's access log stays in the cluster: pyo-agent's traffic store
//! cleans each line there (sensitive query values redacted, the visitor's
//! country looked up, a daily-salted visitor hash made from the address), keeps
//! the cleaned requests in hourly files, and adds up per-minute totals per
//! host and per proxied port. Only the totals go to Pyo. Everything both sides
//! need is here: reading Envoy's lines, the totals and their mergeable
//! sketches (a HyperLogLog for visitors, a log-scale histogram for response
//! times, top lists), the stored rows and the queries Pyo sends for them.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The traffic store's name in pyo-system (its Deployment, Service and
/// pods' `app.kubernetes.io/name`) and port. platform.yaml's network
/// policy of the same name lets in only Vector and the agent.
pub const STORE: &str = "pyo-traffic";
pub const STORE_PORT: u16 = 8090;
/// Vector's DaemonSet in pyo-system, and its pods' `app.kubernetes.io/name`.
pub const VECTOR: &str = "pyo-vector";

/// Query parameters whose values are never kept.
pub const SENSITIVE: [&str; 15] = [
    "token",
    "password",
    "passwd",
    "secret",
    "apikey",
    "api_key",
    "auth",
    "session",
    "signature",
    "email",
    "otp",
    "jwt",
    "credential",
    "code",
    "invite",
];

/// Top lists kept per host and minute.
pub const TOP_PATHS: usize = 50;
pub const TOP_PAGES: usize = 50;
pub const TOP_REFERRERS: usize = 20;
pub const TOP_PARAMS: usize = 20;
pub const TOP_BLOCKED_PATHS: usize = 20;
/// Most rows a query answers with.
pub const MAX_ROWS: u32 = 500;

/// "/a?token=x&page=2" -> ("/a", "/a?token=[redacted]&page=2", ["token","page"], ["token"]).
pub fn split_url(raw: &str) -> (String, String, Vec<String>, Vec<String>) {
    let raw: String = raw.chars().take(2000).collect();
    let (path, query) = match raw.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (raw.clone(), String::new()),
    };
    if query.is_empty() {
        return (path.clone(), path, vec![], vec![]);
    }
    let mut keys = vec![];
    let mut redacted = vec![];
    let parts: Vec<String> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let lower = k.to_ascii_lowercase();
            if !keys.contains(&k.to_owned()) {
                keys.push(k.to_owned());
            }
            if sensitive(&lower) {
                if !redacted.contains(&k.to_owned()) {
                    redacted.push(k.to_owned());
                }
                format!("{k}=[redacted]")
            } else if v.is_empty() {
                k.to_owned()
            } else {
                format!("{k}={v}")
            }
        })
        .collect();
    (path.clone(), format!("{path}?{}", parts.join("&")), keys, redacted)
}

/// A URL as a team's query string setting keeps it: "names" keeps the
/// parameters' names only ("/a?token&page"), "none" drops the query ("/a"),
/// anything else ("keep", or "" from a Pyo before teams chose) keeps it as
/// it is. For a URL `split_url` made (sensitive values already redacted),
/// and again when Pyo reads one.
pub fn shape_url(url: &str, query_strings: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    match query_strings {
        "none" => path.to_owned(),
        "names" => {
            let mut keys: Vec<&str> = vec![];
            for kv in query.split('&').filter(|kv| !kv.is_empty()) {
                let k = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv);
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            if keys.is_empty() { path.to_owned() } else { format!("{path}?{}", keys.join("&")) }
        }
        _ => url.to_owned(),
    }
}

/// Whether a query parameter's values are never kept (by its name, any case).
pub fn sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE.iter().any(|s| lower.contains(s))
}

/// An address as the installation's IP setting keeps it: "none" (nothing),
/// "anonymized" (the last part removed: 203.0.113.0, 2001:db8:85a3::) or
/// "full".
pub fn mask_ip(mode: &str, ip: &str) -> String {
    match (mode, ip.parse::<IpAddr>()) {
        ("full", Ok(a)) => a.to_string(),
        ("anonymized", Ok(IpAddr::V4(v))) => {
            let [a, b, c, _] = v.octets();
            std::net::Ipv4Addr::new(a, b, c, 0).to_string()
        }
        ("anonymized", Ok(IpAddr::V6(v))) => {
            let s = v.segments();
            std::net::Ipv6Addr::new(s[0], s[1], s[2], 0, 0, 0, 0, 0).to_string()
        }
        _ => String::new(),
    }
}

/// Why the proxy answered a request (or ended a connection) itself:
/// "blocked" (an IP or country rule), "rate-limit", or empty.
pub fn refusal(details: &str, termination: &str, status: u64) -> String {
    if [details, termination]
        .iter()
        .any(|d| d.starts_with("rbac_access_denied_matched_policy["))
    {
        return "blocked".into();
    }
    if status == 429 && details.contains("rate_limited") {
        return "rate-limit".into();
    }
    String::new()
}

// ── The gateway's lines ─────────────────────────────────────────────────────

/// One access log line from the gateway (Envoy's JSON, fields in
/// system.rs).
#[derive(Debug, Default, PartialEq)]
pub struct AccessLine {
    pub host: String,
    pub ts_ms: Option<i64>,
    pub peer: String,
    pub forwarded_for: String,
    pub real_ip: String,
    pub user_agent: String,
    pub status: u64,
    pub method: String,
    pub path: String,
    pub content_type: String,
    pub referer: String,
    pub duration_ms: f64,
    pub bytes: u64,
    pub node: String,
    /// The visitor as the proxy saw them (a PROXY protocol header's address,
    /// or the peer).
    pub remote: String,
    /// TCP and UDP ports: bytes from the visitor, the route
    /// (`tcproute/pyo-gateway/<route>/rule/0`), Envoy's flags and how the
    /// connection ended.
    pub bytes_in: u64,
    pub upstream: String,
    pub upstream_host: String,
    pub local_port: u64,
    pub flags: String,
    pub termination: String,
    /// Why the proxy answered itself (a firewall rule, a rate limit).
    pub details: String,
}

/// Which proxied port a connection line is about.
#[derive(Debug, PartialEq)]
pub enum PortKey<'a> {
    /// By its route (TCP connections that reached it).
    Route(&'a str),
    /// By where the proxy listens for it (UDP sessions, which don't name
    /// their route, and TCP connections turned away before one).
    Listen(u16),
}

/// An RFC 3339 time in milliseconds.
pub fn parse_time_ms(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_millisecond())
}

impl AccessLine {
    pub fn read(l: &Value) -> Self {
        // Envoy writes null for a header that wasn't there.
        let s = |k: &str| l[k].as_str().unwrap_or("").to_owned();
        let n = |k: &str| {
            l[k].as_u64()
                .or_else(|| l[k].as_str().and_then(|v| v.parse().ok()))
                .unwrap_or(0)
        };
        AccessLine {
            host: s("host"),
            ts_ms: l["start_time"].as_str().and_then(parse_time_ms),
            peer: s("peer"),
            forwarded_for: s("x_forwarded_for"),
            real_ip: s("x_real_ip"),
            user_agent: s("user_agent"),
            status: n("status"),
            method: s("method"),
            path: s("path"),
            content_type: s("content_type"),
            referer: s("referer"),
            duration_ms: l["duration_ms"]
                .as_f64()
                .or_else(|| l["duration_ms"].as_str().and_then(|v| v.parse().ok()))
                .unwrap_or(0.0),
            bytes: n("bytes"),
            node: s("PyoNode"),
            remote: s("remote"),
            bytes_in: n("bytes_in"),
            upstream: s("upstream"),
            upstream_host: s("upstream_host"),
            local_port: n("local_port"),
            flags: s("flags"),
            termination: s("termination"),
            details: s("details"),
        }
    }

    /// The visitor: the address the proxy placed them at (a PROXY protocol
    /// header's, else the connection's), as its firewall and rate limits
    /// see them; the TCP peer for lines without it. Never X-Forwarded-For
    /// or X-Real-Ip: whoever reaches the proxy could write any address there
    /// (`client_policy_spec`).
    pub fn visitor_ip(&self) -> &str {
        if self.remote.is_empty() {
            &self.peer
        } else {
            &self.remote
        }
    }

    /// A connection to a public TCP or UDP port (no HTTP request in it): its
    /// protocol and which port. TCP connections name their route
    /// (`tcproute/pyo-gateway/<route>/rule/<n>`); UDP sessions don't, but go
    /// somewhere (`upstream_host`); a TCP connection the limit turned away
    /// has neither.
    pub fn port_key(&self) -> Option<(&'static str, PortKey<'_>)> {
        if !self.method.is_empty() || !self.host.is_empty() {
            return None;
        }
        if let Some(rest) = self
            .upstream
            .strip_prefix("tcproute/")
            .or_else(|| self.upstream.strip_prefix("udproute/"))
        {
            let proto = if self.upstream.starts_with("tcp") { "TCP" } else { "UDP" };
            // <namespace>/<route>/rule/<n>
            let route = rest.split('/').nth(1)?;
            return Some((proto, PortKey::Route(route)));
        }
        let port = u16::try_from(self.local_port).ok().filter(|p| *p > 0)?;
        if !self.upstream.is_empty() {
            return None;
        }
        Some((
            if self.upstream_host.is_empty() { "TCP" } else { "UDP" },
            PortKey::Listen(port),
        ))
    }

    /// How a connection went: "ok", "blocked" (an IP rule), "limited" (the
    /// connection limit) or "unreachable" (the service didn't answer).
    pub fn outcome(&self) -> &'static str {
        let t = self.termination.to_ascii_lowercase();
        let f = self.flags.as_str();
        if t.contains("rbac") {
            "blocked"
        } else if t.contains("limit")
            || f.contains("UO")
            || f.contains("OL")
            || (self.upstream.is_empty() && self.upstream_host.is_empty())
        {
            "limited"
        } else if ["UF", "UH", "URX", "NR", "UMSDR"]
            .iter()
            .any(|x| f.split(',').any(|y| y == *x))
        {
            "unreachable"
        } else {
            "ok"
        }
    }
}

/// A port's key in the totals and the context: its route's name, or
/// "<protocol>/<listen port>".
pub fn port_key_string(proto: &str, key: &PortKey<'_>) -> String {
    match key {
        PortKey::Route(r) => (*r).to_owned(),
        PortKey::Listen(p) => format!("{proto}/{p}"),
    }
}

// ── Sketches ────────────────────────────────────────────────────────────────

/// HyperLogLog precision: 2^11 registers, about 2.3% error. Registers are
/// kept sparse (only the ones set) and merge by taking the larger, so
/// minutes add up to hours and hours to months without counting a visitor
/// twice (ClickHouse's `maxMap`).
pub const HLL_P: u32 = 11;
pub const HLL_M: usize = 1 << HLL_P;

/// A visitor hash's register and its value (the leading zeros after the
/// register bits, plus one).
pub fn hll_register(hash: u64) -> (u16, u8) {
    let idx = (hash >> (64 - HLL_P)) as u16;
    let rest = hash << HLL_P;
    let rank = (rest.leading_zeros().min(64 - HLL_P) + 1) as u8;
    (idx, rank)
}

/// Sparse HyperLogLog registers (register -> value).
pub type Hll = BTreeMap<u16, u8>;

pub fn hll_insert(h: &mut Hll, hash: u64) {
    let (i, r) = hll_register(hash);
    let e = h.entry(i).or_insert(0);
    if r > *e {
        *e = r;
    }
}

pub fn hll_merge(into: &mut Hll, from: &Hll) {
    for (i, r) in from {
        let e = into.entry(*i).or_insert(0);
        if r > e {
            *e = *r;
        }
    }
}

/// How many distinct hashes the registers saw (HyperLogLog's estimate,
/// linear counting while most registers are empty). Pyo's queries work
/// out the same in SQL (`hll_sql`).
pub fn hll_estimate(h: &Hll) -> f64 {
    let m = HLL_M as f64;
    let set: Vec<u8> = h.values().copied().filter(|v| *v > 0).collect();
    let zeros = m - set.len() as f64;
    let sum = zeros + set.iter().map(|v| 2f64.powi(-(*v as i32))).sum::<f64>();
    let alpha = 0.7213 / (1.0 + 1.079 / m);
    let e = alpha * m * m / sum;
    if e <= 2.5 * m && zeros > 0.0 {
        m * (m / zeros).ln()
    } else {
        e
    }
}

/// The same estimate in ClickHouse SQL, for a `Map(UInt16, UInt8)`
/// expression of merged registers.
pub fn hll_sql(registers: &str) -> String {
    let m = HLL_M;
    let r = registers;
    let e = format!(
        "({m} * {m} * (0.7213 / (1 + 1.079 / {m})) / (({m} - length({r})) + arraySum(x -> pow(2, -toInt32(x)), mapValues({r}))))"
    );
    format!("if({e} <= 2.5 * {m} AND length({r}) < {m}, {m} * log({m} / ({m} - length({r}))), {e})")
}

/// Response time buckets: 0 for under a millisecond, then each 25% longer
/// than the one before, up to about 15 minutes.
pub const LATENCY_GROWTH: f64 = 1.25;
pub const LATENCY_BUCKETS: u16 = 62;

pub fn latency_bucket(ms: f64) -> u16 {
    if ms.is_nan() || ms < 1.0 {
        return 0;
    }
    ((ms.ln() / LATENCY_GROWTH.ln()).floor() as i64 + 1).clamp(1, LATENCY_BUCKETS as i64) as u16
}

/// A bucket's time in milliseconds: the middle of its range (geometric).
pub fn latency_of(bucket: u16) -> f64 {
    if bucket == 0 {
        return 0.5;
    }
    let lo = LATENCY_GROWTH.powi(bucket as i32 - 1);
    lo * LATENCY_GROWTH.sqrt()
}

/// A quantile (0..1) of a histogram (bucket -> count), in milliseconds.
pub fn latency_quantile(hist: &BTreeMap<u16, u64>, q: f64) -> f64 {
    let total: u64 = hist.values().sum();
    if total == 0 {
        return 0.0;
    }
    let want = (q * total as f64).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (b, n) in hist {
        seen += n;
        if seen >= want {
            return latency_of(*b);
        }
    }
    hist.keys().last().map(|b| latency_of(*b)).unwrap_or(0.0)
}

/// The top keys of a stream with bounded memory (Space-Saving): at most
/// `cap` keys are counted; a new key takes the place of the smallest,
/// starting from its count. Keys that matter keep their exact counts.
#[derive(Debug, Clone)]
pub struct TopK {
    cap: usize,
    counts: HashMap<String, u64>,
}

impl TopK {
    pub fn new(cap: usize) -> Self {
        TopK { cap: cap.max(1), counts: HashMap::new() }
    }

    pub fn add(&mut self, key: &str, n: u64) {
        if let Some(c) = self.counts.get_mut(key) {
            *c += n;
            return;
        }
        if self.counts.len() < self.cap {
            self.counts.insert(key.to_owned(), n);
            return;
        }
        let Some((min_key, min)) = self
            .counts
            .iter()
            .min_by_key(|(_, c)| **c)
            .map(|(k, c)| (k.clone(), *c))
        else {
            return;
        };
        self.counts.remove(&min_key);
        self.counts.insert(key.to_owned(), min + n);
    }

    /// The `n` largest, as a map.
    pub fn top(&self, n: usize) -> BTreeMap<String, u64> {
        let mut v: Vec<(&String, &u64)> = self.counts.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.into_iter().take(n).map(|(k, c)| (k.clone(), *c)).collect()
    }

    pub fn len(&self) -> usize {
        self.counts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}

// ── Totals (cluster -> Pyo) ─────────────────────────────────────────────────

/// One host's website traffic in one minute. Maps add up across minutes
/// (ClickHouse's `sumMap`), `visitors` and `wafVisitors` merge as
/// HyperLogLogs (`maxMap`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostTotals {
    /// The minute's start, in seconds since 1970.
    pub minute: i64,
    pub host: String,
    /// The service the requests went to, on a hostname whose paths go to
    /// several ([`TrafficContext::paths`]); "" otherwise (the hostname's).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub service_id: String,
    pub requests: u64,
    pub pageviews: u64,
    pub bytes: u64,
    /// 5xx answers.
    pub errors: u64,
    /// All requests' time, for averages.
    pub duration_ms: f64,
    /// "2xx".."5xx" (and "1xx", "0xx" for none) -> requests.
    pub statuses: BTreeMap<String, u64>,
    pub methods: BTreeMap<String, u64>,
    /// Response time bucket (`latency_bucket`) -> requests.
    pub latency: BTreeMap<u16, u64>,
    pub visitors: Hll,
    /// Country code ("" unknown) -> requests.
    pub countries: BTreeMap<String, u64>,
    /// Top paths -> requests.
    pub paths: BTreeMap<String, u64>,
    /// Top page URLs (redacted) -> page views.
    pub pages: BTreeMap<String, u64>,
    /// Top referring sites -> page views ("Direct" for none).
    pub referrers: BTreeMap<String, u64>,
    pub devices: BTreeMap<String, u64>,
    pub browsers: BTreeMap<String, u64>,
    /// Top query parameter names -> requests (values never).
    pub params: BTreeMap<String, u64>,
    /// The server that answered -> requests, 5xx answers.
    pub servers: BTreeMap<String, u64>,
    pub server_errors: BTreeMap<String, u64>,
    /// "<server>|<latency bucket>" -> requests.
    pub server_latency: BTreeMap<String, u64>,
    /// The firewall's refusals: rule (or "blocked", "rate-limit", "cdn")
    /// -> requests, by country and top paths.
    pub blocked: BTreeMap<String, u64>,
    pub blocked_countries: BTreeMap<String, u64>,
    pub blocked_paths: BTreeMap<String, u64>,
    /// The WAF's findings: rule number -> requests it blocked, or only
    /// logged; "b:<key>" or "l:<key>" by country and top paths.
    pub waf_blocked: BTreeMap<String, u64>,
    pub waf_logged: BTreeMap<String, u64>,
    pub waf_countries: BTreeMap<String, u64>,
    pub waf_paths: BTreeMap<String, u64>,
    pub waf_visitors: Hll,
}

/// One proxied TCP or UDP port's connections in one minute.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PortTotals {
    pub minute: i64,
    /// The route's name, or "<protocol>/<listen port>" (`port_key_string`).
    pub key: String,
    pub protocol: String,
    pub connections: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Duration bucket -> connections.
    pub duration: BTreeMap<u16, u64>,
    /// "ok", "blocked", "limited", "unreachable" -> connections.
    pub outcomes: BTreeMap<String, u64>,
    pub countries: BTreeMap<String, u64>,
    pub visitors: Hll,
    /// Rule -> connections the firewall refused, and by country.
    pub blocked: BTreeMap<String, u64>,
    pub blocked_countries: BTreeMap<String, u64>,
}

/// What a cluster sends: its finished minutes, and the WAF's rule
/// messages seen in them (rule number -> message).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TotalsBatch {
    /// The batch's id, so Pyo stores a batch sent twice once.
    pub id: String,
    pub hosts: Vec<HostTotals>,
    pub ports: Vec<PortTotals>,
    pub waf_messages: BTreeMap<String, String>,
}

// ── What a cluster needs from Pyo ───────────────────────────────────────────

/// Pyo's view of a cluster's traffic, sent to its traffic store: which
/// hosts and ports are services' (other traffic isn't kept), each
/// service's firewall (to name the rule behind a refusal), and how much is
/// kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrafficContext {
    /// Host -> service id.
    pub hosts: BTreeMap<String, String>,
    /// Hostnames whose paths go to several services: host -> (path, service
    /// id), longest path first.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub paths: BTreeMap<String, Vec<(String, String)>>,
    /// Port key (`port_key_string`) -> (service id, public port).
    pub ports: BTreeMap<String, (String, u16)>,
    /// Service id -> its firewall.
    pub firewalls: BTreeMap<String, crate::firewall::Firewall>,
    /// What's kept of visitors' addresses: "none", "anonymized" or "full"
    /// (the team's setting within the installation's limit).
    pub ip_mode: String,
    /// What's kept of query strings: "keep", "names" or "none" (the team's
    /// setting within the installation's limit). Empty from a Pyo before
    /// teams chose: kept.
    pub query_strings: String,
    /// No requests or connections are kept here (the team turned request
    /// logs or observability off). Totals still go to Pyo.
    pub request_logs_off: bool,
    /// The team's limits on what's kept here.
    pub max_mb: u64,
    pub max_days: u32,
}

impl TrafficContext {
    /// The service a request to `host` at `path` went to: the longest path
    /// of the hostname it's under, else the hostname's.
    pub fn service_of(&self, host: &str, path: &str) -> Option<&String> {
        self.paths
            .get(host)
            .and_then(|ps| {
                ps.iter()
                    .find(|(p, _)| crate::rules::under(path, p))
                    .map(|(_, s)| s)
            })
            .or_else(|| self.hosts.get(host))
    }

    /// Whether `host` is one of the services'.
    pub fn knows(&self, host: &str) -> bool {
        self.hosts.contains_key(host) || self.paths.contains_key(host)
    }
}

/// What's kept by default: 1 GB or 7 days, whichever comes first.
pub const DEFAULT_MAX_MB: u64 = 1024;
pub const DEFAULT_MAX_DAYS: u32 = 7;
/// The team setting's bounds.
pub const MAX_MB_RANGE: (u64, u64) = (100, 20 * 1024);
pub const MAX_DAYS_RANGE: (u32, u32) = (1, 31);

/// A team's limits (`preferences.requestLogs`), within bounds.
pub fn limits_from(prefs: &Value) -> (u64, u32) {
    let r = &prefs["requestLogs"];
    let mb = r["maxMb"]
        .as_u64()
        .unwrap_or(DEFAULT_MAX_MB)
        .clamp(MAX_MB_RANGE.0, MAX_MB_RANGE.1);
    let days = r["maxDays"]
        .as_u64()
        .map(|d| d as u32)
        .unwrap_or(DEFAULT_MAX_DAYS)
        .clamp(MAX_DAYS_RANGE.0, MAX_DAYS_RANGE.1);
    (mb, days)
}

// ── Kept requests and queries (Pyo -> cluster) ──────────────────────────────

/// One request as the cluster keeps it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RequestRow {
    pub at: i64,
    pub host: String,
    /// The service it went to (several share a hostname by path).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub service_id: String,
    pub method: String,
    pub path: String,
    /// The path and query, sensitive values redacted.
    pub url: String,
    pub status: u16,
    pub duration_ms: f64,
    pub bytes: u64,
    pub country: String,
    pub referrer: String,
    pub device: String,
    pub browser: String,
    /// As the IP setting keeps it ("" for none).
    pub ip: String,
    pub user_agent: String,
    pub server: String,
    /// The firewall rule that refused it ("" when it got through).
    pub blocked_by: String,
    /// The WAF blocked it.
    pub waf: bool,
}

/// One connection to a proxied port.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionRow {
    pub at: i64,
    pub key: String,
    pub service_id: String,
    pub port: u16,
    pub protocol: String,
    pub ip: String,
    pub country: String,
    pub duration_ms: f64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub outcome: String,
    pub rule: String,
    pub server: String,
}

/// One WAF finding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WafRow {
    pub at: i64,
    pub host: String,
    /// The service it was for (several share a hostname by path).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub service_id: String,
    pub method: String,
    pub path: String,
    pub url: String,
    pub ip: String,
    pub country: String,
    pub blocked: bool,
    pub rules: Vec<u64>,
    pub messages: Vec<String>,
    pub severity: String,
    pub server: String,
}

/// Which kept rows Pyo asks a cluster for: newest first, before
/// `before_ms`, back to `since_ms`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrafficQuery {
    /// "requests", "connections" or "waf" (not `kind`: that's the
    /// command's tag).
    pub rows: String,
    /// Requests and WAF findings: only these hosts.
    pub hosts: Vec<String>,
    /// And only these services' (on a hostname whose paths go to several).
    /// Rows kept before rows said their service pass.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
    /// Connections: only these port keys.
    pub ports: Vec<String>,
    pub since_ms: i64,
    pub before_ms: i64,
    pub limit: u32,
    /// "1xx".."5xx".
    pub status: Option<String>,
    pub method: Option<String>,
    /// Paths starting with this.
    pub path: Option<String>,
    /// Text in the host or URL (any case).
    pub q: Option<String>,
    /// Requests the firewall refused only.
    pub blocked: bool,
    /// Connections that ended so ("blocked", …).
    pub outcome: Option<String>,
}

/// A cluster's answer: rows (newest first), and whether there are older
/// ones (`more`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrafficAnswer {
    pub rows: Vec<Value>,
    pub more: bool,
    /// The oldest time the cluster still keeps (ms), when it has any.
    pub kept_since: Option<i64>,
    /// It stopped reading before the end of the range (too much to read
    /// in one go); the rows are still the newest that match.
    pub partial: bool,
}

impl TrafficQuery {
    pub fn limit(&self) -> usize {
        self.limit.clamp(1, MAX_ROWS) as usize
    }

    fn text(&self) -> Option<String> {
        self.q
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| q.to_lowercase())
    }

    fn status_class(&self) -> Option<u16> {
        self.status
            .as_deref()
            .and_then(|s| s.strip_suffix("xx"))
            .and_then(|d| d.parse::<u16>().ok())
            .filter(|d| (1..=5).contains(d))
    }

    fn service_ok(&self, service_id: &str) -> bool {
        self.services.is_empty()
            || service_id.is_empty()
            || self.services.iter().any(|s| s == service_id)
    }

    pub fn matches_request(&self, r: &RequestRow) -> bool {
        (self.hosts.is_empty() || self.hosts.iter().any(|h| h == &r.host))
            && self.service_ok(&r.service_id)
            && self.status_class().is_none_or(|c| r.status / 100 == c)
            && self
                .method
                .as_deref()
                .filter(|m| !m.is_empty())
                .is_none_or(|m| r.method.eq_ignore_ascii_case(m))
            && self
                .path
                .as_deref()
                .filter(|p| !p.is_empty())
                .is_none_or(|p| r.path.starts_with(p))
            && (!self.blocked || !r.blocked_by.is_empty())
            && self.text().is_none_or(|q| {
                r.url.to_lowercase().contains(&q) || r.host.to_lowercase().contains(&q)
            })
    }

    pub fn matches_connection(&self, r: &ConnectionRow) -> bool {
        (self.ports.is_empty() || self.ports.iter().any(|k| k == &r.key))
            && self
                .outcome
                .as_deref()
                .filter(|o| !o.is_empty())
                .is_none_or(|o| r.outcome == o)
    }

    pub fn matches_waf(&self, r: &WafRow) -> bool {
        (self.hosts.is_empty() || self.hosts.iter().any(|h| h == &r.host))
            && self.service_ok(&r.service_id)
            && self
                .path
                .as_deref()
                .filter(|p| !p.is_empty())
                .is_none_or(|p| r.path.starts_with(p))
            && self.text().is_none_or(|q| {
                r.url.to_lowercase().contains(&q) || r.host.to_lowercase().contains(&q)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The proxy's access log, as Vector sends it: every field analytics uses
    /// comes from a field the proxy writes.
    #[test]
    fn access_log_fields() {
        let envoy = json!({ "bytes": 354, "client_ip": "203.0.113.9", "content_type": "text/html; charset=utf-8", "duration_ms": 6, "host": "shop.example.com",
            "method": "GET", "path": "/x?a=1", "peer": "10.42.0.1", "referer": "https://a.example/", "start_time": "2026-11-06T12:28:32.253Z", "status": 200,
            "user_agent": "Mozilla/5", "x_forwarded_for": "203.0.113.9,10.0.2.2", "x_real_ip": null, "PyoNode": "sg-1" });
        let a = AccessLine::read(&envoy);
        assert_eq!(
            a,
            AccessLine {
                host: "shop.example.com".into(),
                ts_ms: Some(1793968112253),
                peer: "10.42.0.1".into(),
                forwarded_for: "203.0.113.9,10.0.2.2".into(),
                real_ip: String::new(),
                user_agent: "Mozilla/5".into(),
                status: 200,
                method: "GET".into(),
                path: "/x?a=1".into(),
                content_type: "text/html; charset=utf-8".into(),
                referer: "https://a.example/".into(),
                duration_ms: 6.0,
                bytes: 354,
                node: "sg-1".into(),
                ..Default::default()
            }
        );
        assert_eq!(a.port_key(), None, "a website request");
        // Whatever X-Forwarded-For says, the visitor is who the proxy saw.
        assert_eq!(a.visitor_ip(), "10.42.0.1");
        // Every key the proxy writes is one this reads (and the reverse).
        let mut written: Vec<&str> = crate::system::ACCESS_LOG_FIELDS.iter().map(|(k, _)| *k).collect();
        written.push(crate::system::ACCESS_LOG_CONTENT_TYPE.0);
        written.sort();
        let mut read = vec![
            "host", "start_time", "peer", "x_forwarded_for", "x_real_ip", "user_agent", "status", "method", "path",
            "content_type", "referer", "duration_ms", "bytes", "remote", "local_port", "bytes_in", "upstream",
            "upstream_host", "flags", "termination", "details",
        ];
        read.sort();
        assert_eq!(written, read);
    }

    #[test]
    fn connection_lines() {
        // Envoy's line for a TCP connection: no HTTP fields.
        let tcp = AccessLine::read(
            &json!({ "start_time": "2026-11-06T12:28:32.253Z", "peer": "10.0.2.2", "remote": "203.0.113.9", "local_port": "16379", "bytes": "120", "bytes_in": 64,
            "duration_ms": 5012, "upstream": "tcproute/pyo-gateway/shop-cache-tcp-6379/rule/0", "flags": "-", "termination": null, "host": null, "method": null }),
        );
        assert_eq!(tcp.port_key(), Some(("TCP", PortKey::Route("shop-cache-tcp-6379"))));
        assert_eq!(
            (tcp.bytes_in, tcp.bytes, tcp.duration_ms, tcp.remote.as_str(), tcp.outcome()),
            (64, 120, 5012.0, "203.0.113.9", "ok")
        );
        // As Envoy Gateway v1.9.3 logs them: a UDP session (no route named,
        // after a minute without packets), and a TCP connection the limit
        // turned away (nothing upstream).
        let udp = AccessLine::read(
            &json!({ "bytes": 123, "bytes_in": 55, "duration_ms": 60004, "flags": "-", "host": null, "local_port": 10053, "method": null, "peer": "10.0.2.2", "remote": "10.0.2.2", "status": 0, "upstream": null, "upstream_host": "10.43.152.148:53" }),
        );
        assert_eq!((udp.port_key(), udp.outcome()), (Some(("UDP", PortKey::Listen(10053))), "ok"));
        let limited = AccessLine::read(
            &json!({ "bytes": 0, "bytes_in": 0, "duration_ms": 0, "flags": "-", "local_port": 6379, "peer": "10.0.2.2", "upstream": null, "upstream_host": null }),
        );
        assert_eq!((limited.port_key(), limited.outcome()), (Some(("TCP", PortKey::Listen(6379))), "limited"));
        let outcome = |flags: &str, termination: &str| {
            AccessLine {
                upstream: "tcproute/x/y/rule/0".into(),
                flags: flags.into(),
                termination: termination.into(),
                ..Default::default()
            }
            .outcome()
        };
        assert_eq!(outcome("-", "rbac_access_denied_matched_policy[deny]"), "blocked");
        assert_eq!(outcome("UF,URX", ""), "unreachable");
        assert_eq!(outcome("UO", ""), "limited");
        assert_eq!(port_key_string("UDP", &PortKey::Listen(10053)), "UDP/10053");
        assert_eq!(port_key_string("TCP", &PortKey::Route("r")), "r");
    }

    #[test]
    fn visitors_are_who_the_proxy_saw() {
        let line = |peer: &str, remote: &str, xff: &str, real: &str| AccessLine {
            peer: peer.into(),
            remote: remote.into(),
            forwarded_for: xff.into(),
            real_ip: real.into(),
            ..Default::default()
        };
        assert_eq!(line("203.0.113.7", "203.0.113.7", "1.2.3.4", "").visitor_ip(), "203.0.113.7");
        assert_eq!(
            line("100.64.0.103", "100.64.0.103", "198.51.100.9", "198.51.100.10").visitor_ip(),
            "100.64.0.103"
        );
        assert_eq!(line("10.0.2.2", "198.51.100.9", "6.6.6.6", "").visitor_ip(), "198.51.100.9");
        assert_eq!(line("203.0.113.7", "", "6.6.6.6", "").visitor_ip(), "203.0.113.7");
    }

    #[test]
    fn redacts_sensitive_values() {
        let (path, url, keys, redacted) = split_url("/reset?email=a@b.c&token=xyz&page=2");
        assert_eq!(path, "/reset");
        assert_eq!(url, "/reset?email=[redacted]&token=[redacted]&page=2");
        assert_eq!(keys, vec!["email", "token", "page"]);
        assert_eq!(redacted, vec!["email", "token"]);
        assert_eq!(split_url("/signup?invite=abc").1, "/signup?invite=[redacted]");
        assert_eq!(split_url("/").1, "/");
        assert_eq!(split_url("/a?x").1, "/a?x");
        assert!(sensitive("X-Api_Key") && !sensitive("page"));
    }

    #[test]
    fn ip_masks() {
        assert_eq!(mask_ip("none", "203.0.113.7"), "");
        assert_eq!(mask_ip("anonymized", "203.0.113.7"), "203.0.113.0");
        assert_eq!(mask_ip("anonymized", "2001:db8:85a3:1:2:3:4:5"), "2001:db8:85a3::");
        assert_eq!(mask_ip("full", "203.0.113.7"), "203.0.113.7");
        assert_eq!(mask_ip("full", "not-an-ip"), "");
    }

    #[test]
    fn refusals() {
        assert_eq!(refusal("rbac_access_denied_matched_policy[DENY]", "", 403), "blocked");
        assert_eq!(refusal("", "rbac_access_denied_matched_policy[none]", 0), "blocked");
        assert_eq!(refusal("local_rate_limited", "", 429), "rate-limit");
        assert_eq!(refusal("via_upstream", "", 403), "");
    }

    fn splitmix(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// A handful of visitors reads as exact; more are within a few
    /// percent; merging minutes counts a visitor once.
    #[test]
    fn hll_counts_and_merges() {
        for n in [1u64, 7, 100, 1000, 20_000, 300_000] {
            let mut h = Hll::new();
            for i in 0..n {
                hll_insert(&mut h, splitmix(i));
            }
            let e = hll_estimate(&h);
            let err = (e - n as f64).abs() / n as f64;
            let ok = if n < 100 { (e - n as f64).abs() < 1.0 } else { err < 0.05 };
            assert!(ok, "n={n} estimate={e}");
        }
        // Two minutes with the same 500 visitors and 500 new ones: 1000.
        let (mut a, mut b) = (Hll::new(), Hll::new());
        for i in 0..500 {
            hll_insert(&mut a, splitmix(i));
            hll_insert(&mut b, splitmix(i));
            hll_insert(&mut b, splitmix(10_000 + i));
        }
        hll_merge(&mut a, &b);
        assert!((hll_estimate(&a) - 1000.0).abs() < 30.0, "{}", hll_estimate(&a));
        assert_eq!(hll_estimate(&Hll::new()), 0.0);
        // A register's value is its leading zeros plus one, at most 54.
        assert_eq!(hll_register(0), (0, 54));
        assert_eq!(hll_register(u64::MAX), (2047, 1));
    }

    #[test]
    fn latency_quantiles() {
        assert_eq!(latency_bucket(0.2), 0);
        assert_eq!(latency_bucket(f64::NAN), 0);
        assert_eq!(latency_bucket(1.0), 1);
        assert_eq!(latency_bucket(1e12), LATENCY_BUCKETS);
        let mut h = BTreeMap::new();
        for ms in 1..=1000 {
            *h.entry(latency_bucket(ms as f64)).or_insert(0) += 1;
        }
        for (q, want) in [(0.5, 500.0), (0.95, 950.0), (0.99, 990.0)] {
            let got = latency_quantile(&h, q);
            assert!((got - want).abs() / want < 0.13, "q={q} got={got}");
        }
        assert_eq!(latency_quantile(&BTreeMap::new(), 0.5), 0.0);
    }

    #[test]
    fn top_k_keeps_the_heavy_keys() {
        let mut t = TopK::new(10);
        for i in 0..1000 {
            t.add("/", 1);
            if i % 2 == 0 {
                t.add("/about", 1);
            }
            t.add(&format!("/noise/{i}"), 1);
        }
        let top = t.top(2);
        assert_eq!(top.keys().collect::<Vec<_>>(), ["/", "/about"]);
        assert_eq!(top["/"], 1000);
        assert_eq!(top["/about"], 500);
        assert_eq!(t.len(), 10);
    }

    #[test]
    fn team_limits() {
        assert_eq!(limits_from(&json!({})), (DEFAULT_MAX_MB, DEFAULT_MAX_DAYS));
        assert_eq!(limits_from(&json!({ "requestLogs": { "maxMb": 5, "maxDays": 90 } })), (100, 31));
        assert_eq!(limits_from(&json!({ "requestLogs": { "maxMb": 2048, "maxDays": 3 } })), (2048, 3));
    }

    #[test]
    fn query_strings_as_the_team_keeps_them() {
        let url = "/a?token=[redacted]&page=2&page=3&flag";
        assert_eq!(shape_url(url, "keep"), url);
        assert_eq!(shape_url(url, ""), url, "a Pyo before teams chose");
        assert_eq!(shape_url(url, "names"), "/a?token&page&flag");
        assert_eq!(shape_url(url, "none"), "/a");
        assert_eq!(shape_url("/a", "names"), "/a");
        assert_eq!(shape_url("/a?", "names"), "/a");
        // A context from a Pyo before teams chose keeps what it kept.
        let old: TrafficContext = serde_json::from_value(json!({ "ipMode": "full", "maxMb": 1024, "maxDays": 7 })).unwrap();
        assert_eq!((old.query_strings.as_str(), old.request_logs_off), ("", false));
    }

    /// The query rides in a command tagged by `kind`.
    #[test]
    fn a_query_is_a_command() {
        let q = TrafficQuery { rows: "waf".into(), hosts: vec!["a.example".into()], limit: 5, ..Default::default() };
        let c = crate::Command::QueryTraffic(Box::new(q.clone()));
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!((v["kind"].as_str(), v["rows"].as_str()), (Some("query_traffic"), Some("waf")));
        let back: crate::Command = serde_json::from_value(v).unwrap();
        assert!(matches!(back, crate::Command::QueryTraffic(b) if *b == q));
    }

    #[test]
    fn query_filters() {
        let r = RequestRow {
            host: "shop.example.com".into(),
            method: "POST".into(),
            path: "/api/cart".into(),
            url: "/api/cart?id=1".into(),
            status: 503,
            ..Default::default()
        };
        let q = |v: Value| serde_json::from_value::<TrafficQuery>(v).unwrap();
        assert!(q(json!({ "rows": "requests" })).matches_request(&r));
        assert!(q(json!({ "hosts": ["shop.example.com"], "status": "5xx", "method": "post", "path": "/api" })).matches_request(&r));
        assert!(!q(json!({ "hosts": ["other.example.com"] })).matches_request(&r));
        assert!(!q(json!({ "status": "2xx" })).matches_request(&r));
        assert!(!q(json!({ "path": "/cart" })).matches_request(&r));
        assert!(q(json!({ "q": "CART?ID" })).matches_request(&r));
        assert!(!q(json!({ "blocked": true })).matches_request(&r));
        assert_eq!(q(json!({ "limit": 9000 })).limit(), MAX_ROWS as usize);
        assert_eq!(q(json!({})).limit(), 1);
    }
}
