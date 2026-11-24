//! Per-minute totals of the cleaned rows: per host for websites, per
//! proxied port for connections. Fixed-size sketches (a HyperLogLog for
//! visitors, a histogram for response times) and bounded top lists, so a
//! busy minute costs about as much memory as a quiet one.

use std::collections::{BTreeMap, HashMap};

use pyo_proto::traffic::{
    self as t, HostTotals, PortTotals, TopK, TotalsBatch, hll_insert, latency_bucket,
};

use super::clean::Cleaned;

/// Lines this late after their minute ended still go into it; later ones
/// go into a second row for the same minute, which Pyo adds up.
pub const LATE_SECS: i64 = 30;
/// Top lists track this many times the keys they report, so the counts
/// that are reported are near exact.
const TRACK: usize = 4;
/// Hosts and ports per minute, at most (Pyo said which are services';
/// until it has, a gateway that answers many names can't grow this).
const MAX_KEYS: usize = 2000;
/// Long paths and URLs are cut in the top lists.
const KEY_CHARS: usize = 300;

struct HostAgg {
    t: HostTotals,
    paths: TopK,
    pages: TopK,
    referrers: TopK,
    params: TopK,
    blocked_paths: TopK,
    waf_paths: TopK,
}

impl HostAgg {
    fn new(minute: i64, host: &str, service_id: &str) -> Self {
        HostAgg {
            t: HostTotals { minute, host: host.to_owned(), service_id: service_id.to_owned(), ..Default::default() },
            paths: TopK::new(t::TOP_PATHS * TRACK),
            pages: TopK::new(t::TOP_PAGES * TRACK),
            referrers: TopK::new(t::TOP_REFERRERS * TRACK),
            params: TopK::new(t::TOP_PARAMS * TRACK),
            blocked_paths: TopK::new(t::TOP_BLOCKED_PATHS * TRACK),
            waf_paths: TopK::new(t::TOP_BLOCKED_PATHS * TRACK),
        }
    }

    fn finish(mut self) -> HostTotals {
        self.t.paths = self.paths.top(t::TOP_PATHS);
        self.t.pages = self.pages.top(t::TOP_PAGES);
        self.t.referrers = self.referrers.top(t::TOP_REFERRERS);
        self.t.params = self.params.top(t::TOP_PARAMS);
        self.t.blocked_paths = self.blocked_paths.top(t::TOP_BLOCKED_PATHS);
        self.t.waf_paths = self.waf_paths.top(t::TOP_BLOCKED_PATHS * 2);
        self.t
    }
}

fn bump(m: &mut BTreeMap<String, u64>, k: &str) {
    *m.entry(k.to_owned()).or_insert(0) += 1;
}

fn cut(s: &str) -> String {
    s.chars().take(KEY_CHARS).collect()
}

#[derive(Default)]
pub struct Aggregator {
    /// By minute, host and service (on a hostname whose paths go to
    /// several; "" otherwise).
    hosts: HashMap<(i64, String, String), HostAgg>,
    ports: HashMap<(i64, String), PortTotals>,
    waf_messages: BTreeMap<String, String>,
}

impl Aggregator {
    pub fn add(&mut self, c: &Cleaned) {
        match c {
            Cleaned::Request { row, visitor, pageview, params } => {
                let Some(a) = self.host(row.at, &row.host, &row.service_id) else { return };
                let h = &mut a.t;
                h.requests += 1;
                h.bytes += row.bytes;
                h.duration_ms += row.duration_ms.max(0.0);
                if row.status >= 500 {
                    h.errors += 1;
                }
                bump(&mut h.statuses, &format!("{}xx", row.status / 100));
                bump(&mut h.methods, &row.method);
                let b = latency_bucket(row.duration_ms);
                *h.latency.entry(b).or_insert(0) += 1;
                hll_insert(&mut h.visitors, *visitor);
                bump(&mut h.countries, &row.country);
                if !row.server.is_empty() {
                    bump(&mut h.servers, &row.server);
                    *h.server_latency.entry(format!("{}|{b}", row.server)).or_insert(0) += 1;
                    if row.status >= 500 {
                        bump(&mut h.server_errors, &row.server);
                    }
                }
                if !row.blocked_by.is_empty() {
                    bump(&mut h.blocked, &row.blocked_by);
                    bump(&mut h.blocked_countries, &row.country);
                    a.blocked_paths.add(&cut(&row.path), 1);
                }
                a.paths.add(&cut(&row.path), 1);
                for p in params.iter().take(20) {
                    a.params.add(&cut(p), 1);
                }
                if *pageview {
                    h.pageviews += 1;
                    a.pages.add(&cut(&row.url), 1);
                    if !row.referrer.is_empty() {
                        a.referrers.add(&row.referrer, 1);
                    }
                    bump(&mut h.devices, &row.device);
                    bump(&mut h.browsers, &row.browser);
                }
            }
            Cleaned::Waf { row, visitor } => {
                for (rule, msg) in row.rules.iter().zip(row.messages.iter()) {
                    if self.waf_messages.len() < 500 && !self.waf_messages.contains_key(&rule.to_string()) {
                        self.waf_messages.insert(rule.to_string(), msg.clone());
                    }
                }
                let Some(a) = self.host(row.at, &row.host, &row.service_id) else { return };
                let h = &mut a.t;
                let (map, mark) = if row.blocked { (&mut h.waf_blocked, "b") } else { (&mut h.waf_logged, "l") };
                for rule in &row.rules {
                    bump(map, &rule.to_string());
                }
                bump(&mut h.waf_countries, &format!("{mark}:{}", row.country));
                hll_insert(&mut h.waf_visitors, *visitor);
                a.waf_paths.add(&format!("{mark}:{}", cut(&row.path)), 1);
            }
            Cleaned::Connection { row, visitor } => {
                let minute = row.at.div_euclid(60_000) * 60;
                if !self.ports.contains_key(&(minute, row.key.clone())) && self.ports.len() >= MAX_KEYS {
                    return;
                }
                let p = self.ports.entry((minute, row.key.clone())).or_insert_with(|| PortTotals {
                    minute,
                    key: row.key.clone(),
                    protocol: row.protocol.clone(),
                    ..Default::default()
                });
                p.connections += 1;
                p.bytes_in += row.bytes_in;
                p.bytes_out += row.bytes_out;
                *p.duration.entry(latency_bucket(row.duration_ms)).or_insert(0) += 1;
                bump(&mut p.outcomes, &row.outcome);
                bump(&mut p.countries, &row.country);
                hll_insert(&mut p.visitors, *visitor);
                if row.outcome == "blocked" {
                    bump(&mut p.blocked, if row.rule.is_empty() { "blocked" } else { &row.rule });
                    bump(&mut p.blocked_countries, &row.country);
                }
            }
        }
    }

    fn host(&mut self, at: i64, host: &str, service_id: &str) -> Option<&mut HostAgg> {
        let minute = at.div_euclid(60_000) * 60;
        let key = (minute, host.to_owned(), service_id.to_owned());
        if !self.hosts.contains_key(&key) && self.hosts.len() >= MAX_KEYS {
            return None;
        }
        Some(self.hosts.entry(key).or_insert_with(|| HostAgg::new(minute, host, service_id)))
    }

    /// The minutes that ended at least `LATE_SECS` before `now` (seconds),
    /// taken out. None when there's nothing.
    pub fn take_finished(&mut self, now: i64) -> Option<TotalsBatch> {
        let done = |m: i64| m + 60 + LATE_SECS <= now;
        let hosts: Vec<(i64, String, String)> = self.hosts.keys().filter(|k| done(k.0)).cloned().collect();
        let ports: Vec<(i64, String)> = self.ports.keys().filter(|k| done(k.0)).cloned().collect();
        if hosts.is_empty() && ports.is_empty() {
            return None;
        }
        let mut b = TotalsBatch {
            hosts: hosts.iter().filter_map(|k| self.hosts.remove(k)).map(HostAgg::finish).collect(),
            ports: ports.iter().filter_map(|k| self.ports.remove(k)).collect(),
            waf_messages: std::mem::take(&mut self.waf_messages),
            ..Default::default()
        };
        b.hosts.sort_by(|a, b| (a.minute, &a.host, &a.service_id).cmp(&(b.minute, &b.host, &b.service_id)));
        b.ports.sort_by(|a, b| (a.minute, &a.key).cmp(&(b.minute, &b.key)));
        Some(b)
    }

    pub fn len(&self) -> usize {
        self.hosts.len() + self.ports.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::traffic::{ConnectionRow, RequestRow, WafRow, hll_estimate};

    fn req(at: i64, path: &str, status: u16, visitor: u64, pageview: bool) -> Cleaned {
        Cleaned::Request {
            row: RequestRow {
                at,
                host: "a.example".into(),
                method: "GET".into(),
                path: path.into(),
                url: path.into(),
                status,
                duration_ms: 20.0,
                bytes: 100,
                country: "ID".into(),
                referrer: "Direct".into(),
                device: "Desktop".into(),
                browser: "Chrome".into(),
                server: "sg-1".into(),
                ..Default::default()
            },
            visitor,
            pageview,
            params: vec![],
        }
    }

    #[test]
    fn a_minute_adds_up() {
        let mut a = Aggregator::default();
        // 12:00:10 and 12:00:50 in one minute, 12:01:05 in the next.
        let m = 1_794_000_000_000 / 60_000 * 60_000;
        // Visitor hashes spread over the registers, as real ones are.
        let (v1, v2, v3) = (0x1111_0000_0000_0001, 0x9999_0000_0000_0002, 0x5555_0000_0000_0003);
        a.add(&req(m + 10_000, "/", 200, v1, true));
        a.add(&req(m + 50_000, "/x", 503, v2, false));
        a.add(&req(m + 50_000, "/", 200, v1, true));
        a.add(&req(m + 65_000, "/", 200, v3, true));
        a.add(&Cleaned::Waf {
            row: WafRow { at: m + 1000, host: "a.example".into(), path: "/login".into(), country: "DE".into(), blocked: true, rules: vec![942100], messages: vec!["SQLi".into()], ..Default::default() },
            visitor: 9,
        });
        a.add(&Cleaned::Connection {
            row: ConnectionRow { at: m + 2000, key: "db-tcp-5432".into(), protocol: "TCP".into(), outcome: "blocked".into(), rule: "office".into(), bytes_in: 5, ..Default::default() },
            visitor: 4,
        });
        // The first minute isn't done until 30s after it ends.
        let s = m / 1000;
        assert!(a.take_finished(s + 60 + 29).is_none());
        let b = a.take_finished(s + 60 + 30).unwrap();
        assert_eq!(b.hosts.len(), 1);
        let h = &b.hosts[0];
        assert_eq!((h.minute, h.requests, h.pageviews, h.errors, h.bytes), (s, 3, 2, 1, 300));
        assert_eq!(h.statuses["2xx"], 2);
        assert_eq!(h.statuses["5xx"], 1);
        assert_eq!(hll_estimate(&h.visitors).round(), 2.0);
        assert_eq!(h.paths["/"], 2);
        assert_eq!(h.pages["/"], 2);
        assert_eq!(h.servers["sg-1"], 3);
        assert_eq!(h.server_errors["sg-1"], 1);
        assert_eq!(h.waf_blocked["942100"], 1);
        assert_eq!(h.waf_countries["b:DE"], 1);
        assert_eq!(h.waf_paths["b:/login"], 1);
        assert_eq!(b.waf_messages["942100"], "SQLi");
        assert_eq!(b.ports[0].blocked["office"], 1);
        assert_eq!(b.ports[0].bytes_in, 5);
        // The next minute is still open, then done.
        assert_eq!(a.len(), 1);
        assert_eq!(a.take_finished(s + 200).unwrap().hosts[0].requests, 1);
        assert!(a.take_finished(s + 300).is_none());
    }
}
