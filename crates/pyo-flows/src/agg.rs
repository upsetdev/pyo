//! Conntrack events into per-minute counts, per (port, protocol, client),
//! with bounded memory: at most `max_keys` clients a minute and
//! `max_open` connections remembered for their start time; what doesn't fit
//! is counted as overflow and sent with the batch.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;

use pyo_proto::flows::{FlowPort, FlowRecord};

use crate::conntrack::{Event, Kind};

pub struct Limits {
    pub max_keys: usize,
    pub max_open: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_keys: 20_000,
            max_open: 200_000,
        }
    }
}

type Key = (u16, &'static str, String);

#[derive(Default)]
pub struct Aggregator {
    pub limits: Limits,
    /// The public ports, as (port, protocol number).
    ports: HashSet<(u16, u8)>,
    /// This server's own addresses: connections to them are kept (to
    /// another address, say a pod's, they aren't a public port's).
    local: HashSet<IpAddr>,
    /// conntrack id -> when it started (ms).
    open: HashMap<u32, i64>,
    minutes: BTreeMap<i64, HashMap<Key, FlowRecord>>,
    pub overflow: u64,
}

fn proto_name(p: u8) -> Option<&'static str> {
    match p {
        6 => Some("TCP"),
        17 => Some("UDP"),
        _ => None,
    }
}

impl Aggregator {
    pub fn new(limits: Limits) -> Self {
        Aggregator {
            limits,
            ..Default::default()
        }
    }

    pub fn set_ports(&mut self, ports: &[FlowPort]) {
        self.ports = ports
            .iter()
            .filter_map(|p| {
                Some((
                    p.port,
                    match p.protocol.as_str() {
                        "TCP" => 6,
                        "UDP" => 17,
                        _ => return None,
                    },
                ))
            })
            .collect();
    }

    pub fn set_local(&mut self, addrs: impl IntoIterator<Item = IpAddr>) {
        self.local = addrs.into_iter().collect();
    }

    /// Whether a connection is to one of the cluster's public ports here.
    pub fn wanted(&self, e: &Event) -> bool {
        self.ports.contains(&(e.dst_port, e.proto))
            && (self.local.is_empty() || self.local.contains(&e.dst))
    }

    fn record(&mut self, minute: i64, e: &Event) -> Option<&mut FlowRecord> {
        let proto = proto_name(e.proto)?;
        let key: Key = (e.dst_port, proto, e.src.to_string());
        let bucket = self.minutes.entry(minute).or_default();
        if !bucket.contains_key(&key) && bucket.len() >= self.limits.max_keys {
            self.overflow += 1;
            return None;
        }
        Some(bucket.entry(key).or_insert_with(|| FlowRecord {
            minute,
            port: e.dst_port,
            protocol: proto.into(),
            client: e.src.to_string(),
            ..Default::default()
        }))
    }

    /// One event at `now_ms`.
    pub fn add(&mut self, e: &Event, now_ms: i64) {
        if !self.wanted(e) {
            return;
        }
        let minute = now_ms.div_euclid(60_000) * 60;
        match e.kind {
            Kind::New => {
                if self.open.len() < self.limits.max_open {
                    self.open.insert(e.id, now_ms);
                }
                if let Some(r) = self.record(minute, e) {
                    r.connections += 1;
                }
            }
            Kind::Destroy => {
                let started = self.open.remove(&e.id);
                if let Some(r) = self.record(minute, e) {
                    if let Some(t) = started {
                        r.ended += 1;
                        r.duration_ms += (now_ms - t).max(0) as u64;
                    }
                    r.bytes_in += e.bytes_orig;
                    r.packets_in += e.packets_orig;
                    r.bytes_out += e.bytes_reply;
                    r.packets_out += e.packets_reply;
                }
            }
        }
    }

    /// The records of every minute before `now_ms`'s, taken out.
    pub fn take_finished(&mut self, now_ms: i64) -> Vec<FlowRecord> {
        let current = now_ms.div_euclid(60_000) * 60;
        let done: Vec<i64> = self.minutes.range(..current).map(|(m, _)| *m).collect();
        let mut out = vec![];
        for m in done {
            if let Some(b) = self.minutes.remove(&m) {
                out.extend(b.into_values());
            }
        }
        out.sort_by(|a, b| (a.minute, a.port, &a.client).cmp(&(b.minute, b.port, &b.client)));
        out
    }

    /// Connections started more than `max_age_ms` ago that never ended
    /// (their destroy event was lost): forgotten, so the map stays bounded.
    pub fn expire(&mut self, now_ms: i64, max_age_ms: i64) {
        self.open.retain(|_, t| now_ms - *t < max_age_ms);
    }

    #[cfg(test)]
    pub fn open_count(&self) -> usize {
        self.open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conntrack::{parse_datagram, tests::message};

    fn ev(
        kind: Kind,
        id: u32,
        proto: u8,
        client: [u8; 4],
        dport: u16,
        counters: Option<(u64, u64, u64, u64)>,
    ) -> Event {
        parse_datagram(&message(
            kind,
            id,
            proto,
            client,
            40000 + id as u16,
            [10, 0, 2, 15],
            dport,
            counters,
        ))
        .remove(0)
    }

    fn agg() -> Aggregator {
        let mut a = Aggregator::new(Limits {
            max_keys: 2,
            max_open: 3,
        });
        a.set_ports(&[
            FlowPort {
                port: 6379,
                protocol: "TCP".into(),
            },
            FlowPort {
                port: 53,
                protocol: "UDP".into(),
            },
        ]);
        a.set_local(["10.0.2.15".parse().unwrap()]);
        a
    }

    #[test]
    fn counts_a_minute() {
        let mut a = agg();
        let t = 1_800_000_000_000i64; // a minute boundary
        a.add(
            &ev(Kind::New, 1, 6, [203, 0, 113, 9], 6379, None),
            t + 1_000,
        );
        a.add(
            &ev(Kind::New, 2, 6, [203, 0, 113, 9], 6379, None),
            t + 2_000,
        );
        a.add(
            &ev(
                Kind::Destroy,
                1,
                6,
                [203, 0, 113, 9],
                6379,
                Some((100, 2, 900, 3)),
            ),
            t + 31_000,
        );
        // A destroy for a connection that started before the collector did.
        a.add(
            &ev(
                Kind::Destroy,
                99,
                6,
                [203, 0, 113, 9],
                6379,
                Some((10, 1, 10, 1)),
            ),
            t + 32_000,
        );
        assert!(
            a.take_finished(t + 59_000).is_empty(),
            "the minute isn't over"
        );
        let r = a.take_finished(t + 61_000);
        assert_eq!(r.len(), 1);
        let r = &r[0];
        assert_eq!(
            (r.minute, r.port, r.protocol.as_str(), r.client.as_str()),
            (t / 1000, 6379, "TCP", "203.0.113.9")
        );
        assert_eq!((r.connections, r.ended, r.duration_ms), (2, 1, 30_000));
        assert_eq!(
            (r.bytes_in, r.packets_in, r.bytes_out, r.packets_out),
            (110, 3, 910, 4)
        );
        assert!(a.take_finished(t + 120_000).is_empty(), "taken once");
    }

    #[test]
    fn only_public_ports_on_this_server() {
        let mut a = agg();
        let t = 1_800_000_000_000i64;
        a.add(&ev(Kind::New, 1, 6, [1, 1, 1, 1], 5432, None), t);
        a.add(&ev(Kind::New, 2, 17, [1, 1, 1, 1], 6379, None), t);
        let mut elsewhere = ev(Kind::New, 3, 6, [1, 1, 1, 1], 6379, None);
        elsewhere.dst = "10.42.0.7".parse().unwrap();
        a.add(&elsewhere, t);
        a.add(&ev(Kind::New, 4, 17, [1, 1, 1, 1], 53, None), t);
        let r = a.take_finished(t + 60_000);
        assert_eq!(
            r.iter()
                .map(|x| (x.port, x.protocol.as_str()))
                .collect::<Vec<_>>(),
            vec![(53, "UDP")]
        );
    }

    #[test]
    fn bounded() {
        let mut a = agg();
        let t = 1_800_000_000_000i64;
        for (i, c) in [[1, 1, 1, 1], [2, 2, 2, 2], [3, 3, 3, 3], [4, 4, 4, 4]]
            .iter()
            .enumerate()
        {
            a.add(&ev(Kind::New, i as u32, 6, *c, 6379, None), t);
        }
        assert_eq!(a.take_finished(t + 60_000).len(), 2, "two clients a minute");
        assert_eq!(a.overflow, 2);
        assert_eq!(a.open_count(), 3, "three remembered starts");
        a.expire(t + 3_600_000, 600_000);
        assert_eq!(a.open_count(), 0);
    }
}
