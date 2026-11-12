//! Disk and network from each server's kubelet stats summary
//! (`/api/v1/nodes/{name}/proxy/stats/summary`), the same data the kubelet
//! keeps for itself: no extra component. Fetched for all servers at once with
//! a short timeout; a server whose summary doesn't come back just has no
//! figures this heartbeat.
//!
//! The kubelet's network counters are totals since boot (or since the pod
//! started); they become bytes per second between two heartbeats here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::Client;
use pyo_proto::FsUsage;
use serde_json::Value;

const GIB: f64 = 1073741824.0;
const MIB: f64 = 1048576.0;
const TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Default, Clone, PartialEq)]
pub struct NodeStats {
    pub root_fs: Option<FsUsage>,
    pub image_fs: Option<FsUsage>,
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PodStats {
    pub ephemeral_mi: Option<f64>,
    /// Space used on its persistent volumes, in MiB.
    pub volumes_mi: f64,
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub nodes: HashMap<String, NodeStats>,
    /// (namespace, pod) -> its figures.
    pub pods: HashMap<(String, String), PodStats>,
    /// (namespace, claim) -> the volume's space.
    pub volumes: HashMap<(String, String), FsUsage>,
}

/// Last counter readings, for rates: key -> (bytes, at in seconds).
static LAST: Mutex<Option<HashMap<String, (u64, f64)>>> = Mutex::new(None);

/// Bytes per second between two readings of a cumulative counter. None for
/// the first reading, a counter that went back (the server or pod restarted:
/// it starts again from zero), or readings not apart in time.
pub fn rate(prev: Option<(u64, f64)>, cur: (u64, f64)) -> Option<f64> {
    let (pb, pt) = prev?;
    let (cb, ct) = cur;
    let dt = ct - pt;
    if cb < pb || dt <= 0.0 {
        return None;
    }
    Some(((cb - pb) as f64 / dt * 10.0).round() / 10.0)
}

/// Reads a summary for each server, concurrently. `claims`: (namespace,
/// claim) -> its size in GiB, to tell a volume's own figures (see `parse`).
pub async fn collect(
    client: &Client,
    nodes: &[String],
    claims: &HashMap<(String, String), f64>,
) -> Stats {
    let fetches = nodes.iter().map(|n| async move {
        let req = http::Request::get(format!("/api/v1/nodes/{n}/proxy/stats/summary")).body(vec![]);
        let Ok(req) = req else {
            return (n.clone(), None);
        };
        let body = tokio::time::timeout(TIMEOUT, client.request_text(req)).await;
        let v = match body {
            Ok(Ok(text)) => serde_json::from_str::<Value>(&text).ok(),
            Ok(Err(e)) => {
                tracing::debug!(node = %n, error = %e, "kubelet stats summary failed");
                None
            }
            Err(_) => {
                tracing::debug!(node = %n, "kubelet stats summary timed out");
                None
            }
        };
        (n.clone(), v)
    });
    let summaries = futures::future::join_all(fetches).await;
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let last = last.get_or_insert_with(HashMap::new);
    let mut out = Stats::default();
    for (node, summary) in summaries {
        if let Some(s) = summary {
            parse(&node, &s, claims, last, &mut out);
        }
    }
    out
}

fn fs(v: &Value) -> Option<FsUsage> {
    let used = v["usedBytes"].as_f64()?;
    let cap = v["capacityBytes"].as_f64()?;
    (cap > 0.0).then(|| FsUsage {
        used_gi: (used / GIB * 100.0).round() / 100.0,
        capacity_gi: (cap / GIB * 100.0).round() / 100.0,
    })
}

/// The reading's time in seconds (the kubelet's, else now).
fn secs(v: &Value) -> f64 {
    serde_json::from_value::<Time>(v["time"].clone())
        .map(|t| t.0.as_millisecond() as f64 / 1000.0)
        .unwrap_or_else(|_| pyo_proto::now_ms() as f64 / 1000.0)
}

/// Rates for a network reading, remembered under `key`.
fn net_rates(
    key: &str,
    net: &Value,
    last: &mut HashMap<String, (u64, f64)>,
) -> (Option<f64>, Option<f64>) {
    let t = secs(net);
    let mut one = |dir: &str| {
        let bytes = net[dir].as_u64()?;
        let k = format!("{key}:{dir}");
        let r = rate(last.get(&k).copied(), (bytes, t));
        last.insert(k, (bytes, t));
        r
    };
    (one("rxBytes"), one("txBytes"))
}

/// One server's summary into `out`. A volume's figures count only when they
/// are its own (`dbmetrics::own_disk`): on local-path (k3s's default) a
/// claim is a directory on the server's disk, and the kubelet reports that
/// whole disk for it.
pub fn parse(
    node: &str,
    s: &Value,
    claims: &HashMap<(String, String), f64>,
    last: &mut HashMap<String, (u64, f64)>,
    out: &mut Stats,
) {
    let n = &s["node"];
    let (rx, tx) = net_rates(&format!("node:{node}"), &n["network"], last);
    out.nodes.insert(
        node.to_owned(),
        NodeStats {
            root_fs: fs(&n["fs"]),
            image_fs: fs(&n["runtime"]["imageFs"]),
            net_rx_bps: rx,
            net_tx_bps: tx,
        },
    );
    for p in s["pods"].as_array().into_iter().flatten() {
        let ns = p["podRef"]["namespace"].as_str().unwrap_or("").to_owned();
        let name = p["podRef"]["name"].as_str().unwrap_or("").to_owned();
        let uid = p["podRef"]["uid"].as_str().unwrap_or(&name).to_owned();
        let (rx, tx) = net_rates(&format!("pod:{uid}"), &p["network"], last);
        let mut volumes_mi = 0.0;
        for v in p["volume"].as_array().into_iter().flatten() {
            let Some(claim) = v["pvcRef"]["name"].as_str() else {
                continue;
            };
            let claimed = claims.get(&(ns.clone(), claim.to_owned())).copied();
            if let Some(u) = fs(v).filter(|u| crate::dbmetrics::own_disk(u.capacity_gi, claimed)) {
                volumes_mi += v["usedBytes"].as_f64().unwrap_or(0.0) / MIB;
                out.volumes.insert((ns.clone(), claim.to_owned()), u);
            }
        }
        let ephemeral_mi = p["ephemeral-storage"]["usedBytes"]
            .as_f64()
            .map(|b| (b / MIB * 10.0).round() / 10.0);
        out.pods.insert(
            (ns, name),
            PodStats {
                ephemeral_mi,
                volumes_mi: (volumes_mi * 10.0).round() / 10.0,
                net_rx_bps: rx,
                net_tx_bps: tx,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counter_rates() {
        assert_eq!(rate(None, (1000, 10.0)), None, "first reading");
        assert_eq!(rate(Some((1000, 10.0)), (3000, 20.0)), Some(200.0));
        assert_eq!(
            rate(Some((5000, 10.0)), (100, 20.0)),
            None,
            "counter reset (restart)"
        );
        assert_eq!(rate(Some((1000, 20.0)), (2000, 20.0)), None, "same time");
        assert_eq!(
            rate(Some((1000, 20.0)), (2000, 10.0)),
            None,
            "time went back"
        );
        assert_eq!(rate(Some((1000, 10.0)), (1000, 25.0)), Some(0.0), "idle");
    }

    #[test]
    fn summary_parsing_and_rates() {
        let at = |t: &str, rx: u64, tx: u64, pod_rx: u64| {
            json!({
                "node": {
                    "nodeName": "sg-a",
                    "network": { "time": t, "rxBytes": rx, "txBytes": tx },
                    "fs": { "usedBytes": 10u64 * 1073741824, "capacityBytes": 40u64 * 1073741824 },
                    "runtime": { "imageFs": { "usedBytes": 5u64 * 1073741824, "capacityBytes": 40u64 * 1073741824 } }
                },
                "pods": [{
                    "podRef": { "name": "web-1", "namespace": "shop", "uid": "u1" },
                    "network": { "time": t, "rxBytes": pod_rx, "txBytes": 0 },
                    "ephemeral-storage": { "usedBytes": 1048576u64 * 3 },
                    "volume": [
                        { "name": "data", "usedBytes": 1048576u64 * 512, "capacityBytes": 1073741824u64, "pvcRef": { "name": "web-data", "namespace": "shop" } },
                        { "name": "files", "usedBytes": 10u64 * 1073741824, "capacityBytes": 40u64 * 1073741824, "pvcRef": { "name": "web-files", "namespace": "shop" } },
                        { "name": "kube-api-access", "usedBytes": 12000, "capacityBytes": 99999 }
                    ]
                }]
            })
        };
        let claims = HashMap::from([
            (("shop".to_owned(), "web-data".to_owned()), 1.0),
            (("shop".to_owned(), "web-files".to_owned()), 1.0),
        ]);
        let mut last = HashMap::new();
        let mut out = Stats::default();
        parse(
            "sg-a",
            &at("2026-11-06T10:00:00Z", 1000, 500, 100),
            &claims,
            &mut last,
            &mut out,
        );
        let n = &out.nodes["sg-a"];
        assert_eq!(
            n.root_fs,
            Some(FsUsage {
                used_gi: 10.0,
                capacity_gi: 40.0
            })
        );
        assert_eq!(n.image_fs.map(|f| f.used_gi), Some(5.0));
        assert_eq!(
            (n.net_rx_bps, n.net_tx_bps),
            (None, None),
            "no rate from one reading"
        );
        let p = &out.pods[&("shop".to_owned(), "web-1".to_owned())];
        assert_eq!(p.ephemeral_mi, Some(3.0));
        assert_eq!(
            p.volumes_mi, 512.0,
            "only claims count, not the service account token"
        );
        assert_eq!(
            out.volumes[&("shop".to_owned(), "web-data".to_owned())].capacity_gi,
            1.0
        );
        // A local-path claim reports the server's whole disk: not its own.
        assert!(
            !out.volumes
                .contains_key(&("shop".to_owned(), "web-files".to_owned()))
        );

        let mut out = Stats::default();
        parse(
            "sg-a",
            &at("2026-11-06T10:00:15Z", 16000, 2000, 1600),
            &claims,
            &mut last,
            &mut out,
        );
        assert_eq!(out.nodes["sg-a"].net_rx_bps, Some(1000.0));
        assert_eq!(out.nodes["sg-a"].net_tx_bps, Some(100.0));
        assert_eq!(
            out.pods[&("shop".to_owned(), "web-1".to_owned())].net_rx_bps,
            Some(100.0)
        );
    }

    #[test]
    fn missing_parts_stay_empty() {
        let mut out = Stats::default();
        parse(
            "x",
            &json!({ "node": {}, "pods": [] }),
            &HashMap::new(),
            &mut HashMap::new(),
            &mut out,
        );
        assert_eq!(out.nodes["x"], NodeStats::default());
    }
}
