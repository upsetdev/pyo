//! The path a service's traffic takes, for its Traffic panel: its domains,
//! what their DNS resolves to, the servers that take public traffic (entry
//! servers), and its replicas, grouped by region with live health from
//! heartbeats and requests per region from the request logs.
//!
//! How a request is routed (crates/pyo-agent rollout.rs, observability.rs):
//! it's answered by the gateway on the server it reached; from there it goes
//! to a replica on that server, else one in that server's region, else any.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::{Value, json};

use crate::{
    AppState,
    auth::SessionUser,
    error::ApiResult,
    ops::{Role, require_role, team_of},
};

/// What each hostname resolved to, and when (a minute's cache).
type Resolved = Result<Vec<String>, String>;
static DNS: LazyLock<Mutex<HashMap<String, (Instant, Resolved)>>> = LazyLock::new(Default::default);
const DNS_TTL: Duration = Duration::from_secs(60);

async fn resolve(host: &str) -> Resolved {
    if let Some((at, r)) = DNS.lock().unwrap().get(host).cloned()
        && at.elapsed() < DNS_TTL
    {
        return r;
    }
    let r = match tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::lookup_host(format!("{host}:443")),
    )
    .await
    {
        Ok(Ok(addrs)) => {
            let mut ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
            ips.sort();
            ips.dedup();
            Ok(ips)
        }
        Ok(Err(_)) => Err("It doesn't resolve yet.".to_owned()),
        Err(_) => Err("Looking it up timed out.".to_owned()),
    };
    let mut cache = DNS.lock().unwrap();
    if cache.len() > 5000 {
        cache.clear();
    }
    cache.insert(host.to_owned(), (Instant::now(), r.clone()));
    r
}

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub region: String,
    pub ips: Vec<String>,
    pub ready: bool,
    pub public: bool,
    /// Its public IPv4 and IPv6 address, if it has one of each.
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
}

/// A server answers public traffic (runs the gateway, published): its
/// cluster takes public traffic and it isn't a build-only server.
pub fn entry_server(cluster_public: bool, report: &Value) -> bool {
    cluster_public && report["builder"] != json!(true)
}

/// A server's public address of each family, from what its node reports:
/// the ones the agent found first, then the cluster's own addresses.
pub fn public_families(r: &Value) -> (Option<String>, Option<String>) {
    let mut all: Vec<&str> = ["publicIpv4", "publicIpv6", "externalIP"]
        .iter()
        .filter_map(|k| r[*k].as_str())
        .collect();
    all.extend(
        r["addresses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    );
    all.extend(
        ["publicIp", "internalIP"]
            .iter()
            .filter_map(|k| r[*k].as_str()),
    );
    let public: Vec<std::net::IpAddr> = all
        .iter()
        .filter_map(|ip| ip.parse().ok())
        .filter(crate::agent::public_ip)
        .collect();
    (
        public.iter().find(|a| a.is_ipv4()).map(ToString::to_string),
        public.iter().find(|a| a.is_ipv6()).map(ToString::to_string),
    )
}

#[derive(Debug, Clone)]
pub struct Replica {
    pub name: String,
    pub node: String,
    pub ready: bool,
}

/// Where an entry server sends requests, by the cluster's mode: local, its
/// own replicas, else any; region, its region's, else any; spread, any
/// healthy replica.
pub fn serves<'a>(
    entry: &Server,
    servers: &[Server],
    replicas: &'a [Replica],
    mode: &str,
) -> (&'static str, Vec<&'a Replica>) {
    let ready: Vec<&Replica> = replicas.iter().filter(|r| r.ready).collect();
    if mode == pyo_proto::TRAFFIC_SPREAD {
        return ("any", ready);
    }
    if mode == pyo_proto::TRAFFIC_LOCAL {
        let local: Vec<&Replica> = ready
            .iter()
            .copied()
            .filter(|r| r.node == entry.name)
            .collect();
        return if local.is_empty() {
            ("anywhere", ready)
        } else {
            ("server", local)
        };
    }
    let region_of = |node: &str| {
        servers
            .iter()
            .find(|s| s.name == node)
            .map(|s| s.region.as_str())
            .unwrap_or("")
    };
    let same: Vec<&Replica> = ready
        .iter()
        .copied()
        .filter(|r| region_of(&r.node) == entry.region)
        .collect();
    if !same.is_empty() {
        return ("region", same);
    }
    ("anywhere", ready)
}

/// The plain-word warnings of the panel.
/// Spread mode drops the one that's about keeping traffic in its region.
pub fn warnings(
    cluster: &str,
    domains: &[(String, Resolved)],
    servers: &[Server],
    replicas: &[Replica],
    extra_ips: &[String],
    mode: &str,
) -> Vec<String> {
    let spread = mode == pyo_proto::TRAFFIC_SPREAD;
    let mut out = vec![];
    let region_of = |node: &str| {
        servers
            .iter()
            .find(|s| s.name == node)
            .map(|s| s.region.clone())
            .unwrap_or_default()
    };
    let label = |r: &str| {
        if r.is_empty() {
            "servers without a region".to_owned()
        } else {
            r.to_owned()
        }
    };
    let replica_regions: std::collections::BTreeSet<String> = replicas
        .iter()
        .filter(|r| r.ready)
        .map(|r| region_of(&r.node))
        .collect();
    for (host, r) in domains {
        let ips = match r {
            Ok(ips) => ips,
            Err(e) => {
                out.push(format!("{host}: {e}"));
                continue;
            }
        };
        let mut pointed = std::collections::BTreeSet::new();
        let mut strangers = vec![];
        for ip in ips {
            match servers.iter().find(|s| s.ips.contains(ip)) {
                Some(s) => {
                    pointed.insert(s.region.clone());
                    if !s.public {
                        out.push(format!("{host} points at {}, which doesn't take public traffic: requests there aren't answered.", s.name));
                    }
                }
                None if extra_ips.contains(ip) => {}
                None => strangers.push(ip.as_str()),
            }
        }
        if !strangers.is_empty() {
            let them = strangers.join(", ");
            let what = if strangers.len() == 1 {
                "isn't one of"
            } else {
                "aren't"
            };
            out.push(format!(
                "{host} points at {them}, which {what} {cluster}'s servers. Requests there don't reach this service unless it's a load balancer or proxy in front of them."
            ));
        }
        if !spread && pointed.len() == 1 && replica_regions.len() > 1 {
            let only = pointed.iter().next().cloned().unwrap_or_default();
            out.push(format!(
                "{host} only points at {}, but replicas run in {} regions. Visitors everywhere still reach {}; point it at a server in each region (or a load balancer in front of them).",
                label(&only),
                replica_regions.len(),
                label(&only),
            ));
        }
    }
    // Regions whose entry servers have no healthy replica close by.
    let mut regions: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for s in servers.iter().filter(|s| s.public && s.ready) {
        regions.entry(s.region.clone()).or_default().0 += 1;
    }
    for r in replicas.iter().filter(|r| r.ready) {
        regions.entry(region_of(&r.node)).or_default().1 += 1;
    }
    let named = regions.keys().any(|r| !r.is_empty());
    if named && !replicas.is_empty() {
        for (region, (entries, healthy)) in &regions {
            if *entries > 0 && *healthy == 0 {
                out.push(format!("{} takes traffic but has no healthy replica, so its requests go to another region.", label(region)));
            }
        }
        // Lasting imbalance: the spread is soft, so a full or offline
        // region can end up with fewer.
        let counts: Vec<usize> = regions
            .values()
            .filter(|(_, h)| *h > 0)
            .map(|(_, h)| *h)
            .collect();
        if counts.len() > 1
            && counts.iter().max().unwrap_or(&0) - counts.iter().min().unwrap_or(&0) > 1
        {
            out.push("Replicas aren't spread evenly across regions, probably because a region was full or offline. Redeploy once it has room.".to_owned());
        }
    }
    out
}

/// How traffic reaches a service
///
/// Its domains and the addresses they point at, the servers that take
/// public traffic and their regions, where its replicas run, and anything
/// that keeps visitors from reaching it.
#[utoipa::path(get, path = "/services/{id}/traffic", operation_id = "getServiceTraffic", tag = "Services",
    params(("id" = String, Path, description = "The service's id (`svc_…`).")),
    responses(
        (status = 200, description = "Domains, entry servers, replicas and warnings.", body = Object),
        (status = 404, description = "No such service, or it isn't yours to see.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn get(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team = team_of(&mut tx, "services", &id).await?;
    require_role(&mut tx, &user, &team, Role::Viewer).await?;
    let (cluster_id, spec, runtime): (String, Value, Value) =
        sqlx::query_as("SELECT cluster_id, spec, runtime FROM pyo.services WHERE id = $1")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
    let (cluster, caps, mode): (String, Value, Option<String>) =
        sqlx::query_as("SELECT name, capabilities, traffic_mode FROM pyo.clusters WHERE id = $1")
            .bind(&cluster_id)
            .fetch_one(&mut *tx)
            .await?;
    let mode = match mode.as_deref() {
        Some(m @ (pyo_proto::TRAFFIC_LOCAL | pyo_proto::TRAFFIC_SPREAD)) => m.to_owned(),
        _ => pyo_proto::TRAFFIC_REGION.to_owned(),
    };
    let rows: Vec<(String, Value, Option<String>, bool)> = sqlx::query_as(
        "SELECT n.name, n.report, coalesce(n.location, c.location), c.public_traffic FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id
          WHERE n.cluster_id = $1 ORDER BY n.name",
    )
    .bind(&cluster_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let servers: Vec<Server> = rows
        .into_iter()
        .map(|(name, r, region, public)| {
            let mut ips: Vec<String> = [
                "internalIP",
                "externalIP",
                "publicIp",
                "publicIpv4",
                "publicIpv6",
            ]
            .iter()
            .filter_map(|k| r[*k].as_str())
            .map(str::to_owned)
            .collect();
            ips.extend(
                r["addresses"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            ips.retain(|ip| !ip.is_empty());
            let mut seen = std::collections::HashSet::new();
            ips.retain(|ip| seen.insert(ip.clone()));
            let (ipv4, ipv6) = public_families(&r);
            Server {
                name,
                region: region.unwrap_or_default(),
                ips,
                ready: r["status"] == json!("ready"),
                public: entry_server(public, &r),
                ipv4,
                ipv6,
            }
        })
        .collect();
    let replicas: Vec<Replica> = runtime["replicas"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| Replica {
            name: r["name"].as_str().unwrap_or("").to_owned(),
            node: r["server"].as_str().unwrap_or("").to_owned(),
            ready: r["ready"] == json!(true),
        })
        .collect();
    let hosts: Vec<String> = spec["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["hostname"].as_str())
        .map(|h| h.to_lowercase())
        .collect();
    let mut domains = vec![];
    for h in &hosts {
        domains.push((h.clone(), resolve(h).await));
    }
    let lb_ips: Vec<String> = caps["loadBalancerIps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .map(str::to_owned)
        .collect();
    let warnings = warnings(&cluster, &domains, &servers, &replicas, &lb_ips, &mode);

    // Requests per region, from the server that answered each one.
    let stats_available = s.observability.enabled();
    let per_server = s
        .observability
        .per_server(&team, &hosts, 24)
        .await
        .unwrap_or_default();
    let hourly = s
        .observability
        .per_server_hourly(&team, &hosts, 24)
        .await
        .unwrap_or_default();
    let num = |v: &Value| {
        v.as_str()
            .and_then(|x| x.parse::<f64>().ok())
            .or(v.as_f64())
            .unwrap_or(0.0)
    };
    // Each server's own numbers (its share of the region's requests).
    let server_stats = |name: &str| {
        per_server.iter().find(|r| r["node"].as_str() == Some(name)).map(|r| json!({ "requests": num(&r["requests"]) as u64, "errors": num(&r["errors"]) as u64, "p95Ms": num(&r["p95Ms"]) }))
    };
    // Requests per hour of a set of servers, oldest first (24 points).
    let now_h = pyo_proto::now_ms() / 1000 / 3600 * 3600;
    let series = |names: &[&str]| -> Vec<u64> {
        (0..24)
            .map(|i| {
                let h = now_h - (23 - i) * 3600;
                hourly
                    .iter()
                    .filter(|r| {
                        names.contains(&r["node"].as_str().unwrap_or(""))
                            && num(&r["hour"]) as i64 == h
                    })
                    .map(|r| num(&r["requests"]) as u64)
                    .sum()
            })
            .collect()
    };
    let mut by_region: BTreeMap<String, (u64, u64, f64, f64)> = BTreeMap::new();
    for row in &per_server {
        let node = row["node"].as_str().unwrap_or("");
        let Some(region) = servers
            .iter()
            .find(|s| s.name == node)
            .map(|s| s.region.clone())
        else {
            continue;
        };
        let n = row["requests"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .or(row["requests"].as_u64())
            .unwrap_or(0);
        let e = row["errors"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .or(row["errors"].as_u64())
            .unwrap_or(0);
        let avg = row["avgMs"].as_f64().unwrap_or(0.0);
        let p95 = row["p95Ms"].as_f64().unwrap_or(0.0);
        let t = by_region.entry(region).or_default();
        // Weighted average; the worst p95 of the region's servers.
        t.2 = if t.0 + n > 0 {
            (t.2 * t.0 as f64 + avg * n as f64) / (t.0 + n) as f64
        } else {
            0.0
        };
        t.0 += n;
        t.1 += e;
        t.3 = t.3.max(p95);
    }

    // Regions of the servers there are now (requests answered by servers
    // that have since left aren't shown).
    let mut names: Vec<String> = servers.iter().map(|s| s.region.clone()).collect();
    names.sort_by(|a, b| a.is_empty().cmp(&b.is_empty()).then(a.cmp(b)));
    names.dedup();
    let regions: Vec<Value> = names
        .iter()
        .map(|region| {
            let in_region: Vec<Value> = servers
                .iter()
                .filter(|s| &s.region == region)
                .map(|srv| {
                    let (how, to) = serves(srv, &servers, &replicas, &mode);
                    json!({
                        "name": srv.name, "ips": srv.ips, "ipv4": srv.ipv4, "ipv6": srv.ipv6, "ready": srv.ready, "publicTraffic": srv.public,
                        "pointedBy": domains.iter().filter(|(_, r)| r.as_ref().is_ok_and(|ips| ips.iter().any(|ip| srv.ips.contains(ip)))).map(|(h, _)| h).collect::<Vec<_>>(),
                        "sendsTo": if srv.public { json!({ "how": how, "replicas": to.iter().map(|r| &r.name).collect::<Vec<_>>() }) } else { Value::Null },
                        "stats": server_stats(&srv.name),
                    })
                })
                .collect();
            let reps: Vec<Value> = replicas
                .iter()
                .filter(|r| servers.iter().find(|s| s.name == r.node).map(|s| &s.region) == Some(region) || (region.is_empty() && !servers.iter().any(|s| s.name == r.node)))
                .map(|r| json!({ "name": r.name, "server": r.node, "ready": r.ready }))
                .collect();
            let st = by_region.get(region).map(|(n, e, avg, p95)| json!({ "requests": n, "errors": e, "avgMs": (avg * 10.0).round() / 10.0, "p95Ms": p95 }));
            let names: Vec<&str> = servers.iter().filter(|s| &s.region == region).map(|s| s.name.as_str()).collect();
            json!({ "name": region, "servers": in_region, "replicas": reps, "stats": st, "series": if stats_available { json!(series(&names)) } else { Value::Null } })
        })
        .collect();
    Ok(Json(json!({
        "cluster": cluster,
        "mode": mode,
        "domains": domains.iter().map(|(h, r)| match r {
            Ok(ips) => json!({ "hostname": h, "addresses": ips, "error": null }),
            Err(e) => json!({ "hostname": h, "addresses": [], "error": e }),
        }).collect::<Vec<_>>(),
        "regions": regions,
        "warnings": warnings,
        "statsAvailable": stats_available,
        "statsHours": 24,
        // Agents that route in-region and label regions.
        "inRegionRouting": caps["regions"] == json!(true),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_servers_are_every_server_but_build_only_ones_while_public() {
        assert!(entry_server(true, &json!({ "name": "a" })));
        assert!(entry_server(
            true,
            &json!({ "name": "a", "builder": false })
        ));
        assert!(!entry_server(
            true,
            &json!({ "name": "b", "builder": true })
        ));
        assert!(!entry_server(false, &json!({ "name": "a" })));
    }

    fn srv(name: &str, region: &str, ip: &str) -> Server {
        Server {
            name: name.into(),
            region: region.into(),
            ips: vec![ip.into()],
            ready: true,
            public: true,
            ipv4: None,
            ipv6: None,
        }
    }

    #[test]
    fn families() {
        let r = json!({ "internalIP": "10.0.0.5", "externalIP": null, "publicIp": "2001:db8::7", "addresses": ["10.0.0.5", "fd00::5", "203.0.113.7"] });
        // 2001:db8:: is documentation space but parses as global here.
        assert_eq!(
            public_families(&r),
            (Some("203.0.113.7".into()), Some("2001:db8::7".into()))
        );
        let r = json!({ "internalIP": "100.64.0.10", "publicIpv6": "2001:db8::1" });
        assert_eq!(public_families(&r), (None, Some("2001:db8::1".into())));
        assert_eq!(
            public_families(&json!({ "internalIP": "10.0.0.5" })),
            (None, None)
        );
    }
    fn rep(name: &str, node: &str) -> Replica {
        Replica {
            name: name.into(),
            node: node.into(),
            ready: true,
        }
    }

    #[test]
    fn requests_stay_close() {
        let servers = [
            srv("sg-a", "Singapore", "1.1.1.1"),
            srv("sg-b", "Singapore", "1.1.1.2"),
            srv("us-a", "US East", "2.2.2.1"),
        ];
        let replicas = [rep("w-1", "sg-a"), rep("w-2", "us-a")];
        // Regional: the region's replicas, the server's own among them.
        let (how, to) = serves(&servers[0], &servers, &replicas, "region");
        assert_eq!((how, to.len(), to[0].name.as_str()), ("region", 1, "w-1"));
        let (how, to) = serves(&servers[1], &servers, &replicas, "region");
        assert_eq!((how, to.len(), to[0].name.as_str()), ("region", 1, "w-1"));
        // No healthy replica in the region: anywhere.
        let only_us = [rep("w-2", "us-a")];
        assert_eq!(
            serves(&servers[1], &servers, &only_us, "region").0,
            "anywhere"
        );
        // Local: the server's own, else any (not its region first).
        let (how, to) = serves(&servers[0], &servers, &replicas, "local");
        assert_eq!((how, to.len(), to[0].name.as_str()), ("server", 1, "w-1"));
        let (how, to) = serves(&servers[1], &servers, &replicas, "local");
        assert_eq!((how, to.len()), ("anywhere", 2));
        // Global: any healthy replica, from any server.
        let (how, to) = serves(&servers[0], &servers, &replicas, "spread");
        assert_eq!((how, to.len()), ("any", 2));
    }

    #[test]
    fn plain_warnings() {
        let servers = [
            srv("sg-a", "Singapore", "1.1.1.1"),
            srv("us-a", "US East", "2.2.2.1"),
        ];
        let replicas = [rep("w-1", "sg-a"), rep("w-2", "us-a")];
        let w = warnings(
            "main",
            &[("shop.example.com".into(), Ok(vec!["1.1.1.1".into()]))],
            &servers,
            &replicas,
            &[],
            "region",
        );
        assert!(
            w[0].contains("only points at Singapore, but replicas run in 2 regions"),
            "{w:?}"
        );
        let one = [("shop.example.com".into(), Ok(vec!["1.1.1.1".into()]))];
        assert!(
            warnings("main", &one, &servers, &replicas, &[], "spread").is_empty(),
            "spread: one region is fine"
        );
        let w = warnings(
            "main",
            &[("shop.example.com".into(), Ok(vec!["9.9.9.9".into()]))],
            &servers,
            &replicas,
            &[],
            "region",
        );
        assert!(w[0].contains("isn't one of main's servers"));
        assert!(
            warnings(
                "main",
                &[("x.example.com".into(), Ok(vec!["9.9.9.9".into()]))],
                &servers,
                &replicas,
                &["9.9.9.9".into()],
                "region"
            )
            .is_empty()
        );
        let w = warnings("main", &[], &servers, &[rep("w-1", "sg-a")], &[], "region");
        assert!(w[0].starts_with("US East takes traffic but has no healthy replica"));
        let w = warnings(
            "main",
            &[],
            &servers,
            &[
                rep("a", "sg-a"),
                rep("b", "sg-a"),
                rep("c", "sg-a"),
                rep("d", "us-a"),
            ],
            &[],
            "region",
        );
        assert!(w.iter().any(|x| x.contains("aren't spread evenly")));
        // No regions yet: no region warnings.
        let plain = [srv("a", "", "1.1.1.1"), srv("b", "", "1.1.1.2")];
        assert!(warnings("main", &[], &plain, &[rep("w", "a")], &[], "region").is_empty());
    }
}
