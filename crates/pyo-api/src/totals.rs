//! Traffic totals: what each cluster's traffic store adds up per minute
//! (pyo-proto's traffic.rs), the only website and port traffic Pyo keeps.
//! Raw requests and visitors' addresses stay in the clusters.
//!
//! Minutes go into `traffic_1m` and `ports_1m`, kept 3 days (an hour or a
//! day, and the one before it); a materialized view adds them up per hour
//! into `traffic_1h` and `ports_1h`, kept 12 months by default (the
//! installation's retention, or a team's shorter one: retention.rs). All
//! four are AggregatingMergeTree tables of simple aggregates: sums, maps
//! that add up (`sumMap`) and HyperLogLog registers that merge (`maxMap`),
//! so rows for the same key (a late minute, a part not merged yet) add up
//! when read. A batch is stored once, however often the agent sends it
//! (its id is ClickHouse's insert deduplication token).
//!
//! The analytics page, the firewall's and the WAF's pages, port
//! connections and the per-server figures read only these, so they work
//! with a cluster offline. Ranges of an hour or a day read minutes; a week
//! or a month read hours.

use std::collections::{BTreeMap, HashMap};

use pyo_proto::traffic::{
    self as t, HLL_M, HostTotals, LATENCY_BUCKETS, PortTotals, TotalsBatch, TrafficContext, hll_sql,
    latency_quantile,
};
use serde_json::{Value, json};

use crate::error::{ApiError, ApiResult};
use crate::observability::{ClickHouse, Observability, bar_steps};

/// The columns both website tables have, as ClickHouse types.
const TRAFFIC_COLUMNS: &str = "
    ts DateTime('UTC'),
    team_id LowCardinality(String),
    service_id LowCardinality(String),
    host LowCardinality(String),
    cluster_id SimpleAggregateFunction(anyLast, LowCardinality(String)),
    project_id SimpleAggregateFunction(anyLast, LowCardinality(String)),
    requests SimpleAggregateFunction(sum, UInt64),
    pageviews SimpleAggregateFunction(sum, UInt64),
    bytes SimpleAggregateFunction(sum, UInt64),
    errors SimpleAggregateFunction(sum, UInt64),
    duration_ms SimpleAggregateFunction(sum, Float64),
    statuses SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    methods SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    latency SimpleAggregateFunction(sumMap, Map(UInt16, UInt64)),
    visitors SimpleAggregateFunction(maxMap, Map(UInt16, UInt8)),
    countries SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    paths SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    pages SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    referrers SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    devices SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    browsers SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    params SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    servers SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    server_errors SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    server_latency SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    blocked SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    blocked_countries SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    blocked_paths SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    waf_blocked SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    waf_logged SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    waf_countries SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    waf_paths SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    waf_visitors SimpleAggregateFunction(maxMap, Map(UInt16, UInt8))";

const PORT_COLUMNS: &str = "
    ts DateTime('UTC'),
    team_id LowCardinality(String),
    service_id LowCardinality(String),
    port UInt16,
    protocol LowCardinality(String),
    cluster_id SimpleAggregateFunction(anyLast, LowCardinality(String)),
    project_id SimpleAggregateFunction(anyLast, LowCardinality(String)),
    connections SimpleAggregateFunction(sum, UInt64),
    bytes_in SimpleAggregateFunction(sum, UInt64),
    bytes_out SimpleAggregateFunction(sum, UInt64),
    duration SimpleAggregateFunction(sumMap, Map(UInt16, UInt64)),
    outcomes SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    countries SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    visitors SimpleAggregateFunction(maxMap, Map(UInt16, UInt8)),
    blocked SimpleAggregateFunction(sumMap, Map(String, UInt64)),
    blocked_countries SimpleAggregateFunction(sumMap, Map(String, UInt64))";

/// Each column's merge (for the hourly views): sum, sumMap, maxMap or anyLast.
fn merges(columns: &str) -> Vec<(String, &'static str)> {
    columns
        .lines()
        .filter_map(|l| {
            let l = l.trim().trim_end_matches(',');
            let (name, ty) = l.split_once(' ')?;
            let f = if ty.starts_with("SimpleAggregateFunction(sumMap") {
                "sumMap"
            } else if ty.starts_with("SimpleAggregateFunction(maxMap") {
                "maxMap"
            } else if ty.starts_with("SimpleAggregateFunction(sum") {
                "sum"
            } else if ty.starts_with("SimpleAggregateFunction(anyLast") {
                "anyLast"
            } else {
                return None;
            };
            Some((name.to_owned(), f))
        })
        .collect()
}

/// The statements that make the totals' tables and views.
pub fn schema() -> Vec<String> {
    let table = |name: &str, cols: &str, key: &str, part: &str, ttl: &str| {
        format!(
            "CREATE TABLE IF NOT EXISTS pyo_observability.{name} ({cols}) ENGINE = AggregatingMergeTree \
             PARTITION BY {part} ORDER BY ({key}) TTL {ttl} SETTINGS non_replicated_deduplication_window = 1000"
        )
    };
    let view = |name: &str, from: &str, to: &str, cols: &str, keys: &str| {
        let merged = merges(cols)
            .into_iter()
            .map(|(c, f)| format!("{f}({c}) AS {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS pyo_observability.{name} TO pyo_observability.{to} AS \
             SELECT toStartOfHour(m.ts) AS ts, {keys}, {merged} FROM pyo_observability.{from} AS m GROUP BY ts, {keys}"
        )
    };
    vec![
        table("traffic_1m", TRAFFIC_COLUMNS, "team_id, host, ts, service_id", "toYYYYMMDD(ts)", "ts + INTERVAL 3 DAY"),
        table("traffic_1h", TRAFFIC_COLUMNS, "team_id, host, ts, service_id", "toYYYYMM(ts)", "ts + INTERVAL 12 MONTH"),
        view("traffic_1h_mv", "traffic_1m", "traffic_1h", TRAFFIC_COLUMNS, "team_id, service_id, host"),
        table("ports_1m", PORT_COLUMNS, "team_id, service_id, port, protocol, ts", "toYYYYMMDD(ts)", "ts + INTERVAL 3 DAY"),
        table("ports_1h", PORT_COLUMNS, "team_id, service_id, port, protocol, ts", "toYYYYMM(ts)", "ts + INTERVAL 12 MONTH"),
        view("ports_1h_mv", "ports_1m", "ports_1h", PORT_COLUMNS, "team_id, service_id, port, protocol"),
        // The WAF's rule messages, by team (custom rules are the team's own).
        "CREATE TABLE IF NOT EXISTS pyo_observability.waf_rules (team_id LowCardinality(String), rule String, message String, seen DateTime('UTC')) \
         ENGINE = ReplacingMergeTree(seen) ORDER BY (team_id, rule) TTL seen + INTERVAL 12 MONTH"
            .to_owned(),
    ]
}

// ── Taking totals in ────────────────────────────────────────────────────────

/// A batch's limits, so a cluster can't store more than real traffic makes.
const MAX_ROWS: usize = 20_000;
const MAX_KEYS: usize = 250;

fn capped<V: Clone>(m: &BTreeMap<String, V>, max_len: usize) -> BTreeMap<String, V> {
    m.iter()
        .filter(|(k, _)| k.len() <= max_len)
        .take(MAX_KEYS)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn hll(m: &t::Hll) -> BTreeMap<u16, u8> {
    m.iter()
        .filter(|(i, r)| (**i as usize) < HLL_M && **r > 0 && **r <= 64)
        .map(|(i, r)| (*i, *r))
        .collect()
}

fn hist(m: &BTreeMap<u16, u64>) -> BTreeMap<u16, u64> {
    m.iter().filter(|(b, _)| **b <= LATENCY_BUCKETS).map(|(b, n)| (*b, *n)).collect()
}

/// Who a host or port belongs to.
#[derive(Clone, Debug)]
pub struct Owner {
    pub team_id: String,
    pub project_id: String,
    pub service_id: String,
}

/// One host's minute as a `traffic_1m` row.
pub fn host_row(cluster_id: &str, o: &Owner, h: &HostTotals) -> Value {
    json!({
        "ts": h.minute, "team_id": o.team_id, "service_id": o.service_id, "host": h.host,
        "cluster_id": cluster_id, "project_id": o.project_id,
        "requests": h.requests, "pageviews": h.pageviews.min(h.requests), "bytes": h.bytes, "errors": h.errors.min(h.requests),
        "duration_ms": if h.duration_ms.is_finite() { h.duration_ms.max(0.0) } else { 0.0 },
        "statuses": capped(&h.statuses, 8), "methods": capped(&h.methods, 16), "latency": hist(&h.latency),
        "visitors": hll(&h.visitors), "countries": capped(&h.countries, 8),
        "paths": capped(&h.paths, 400), "pages": capped(&h.pages, 400), "referrers": capped(&h.referrers, 260),
        "devices": capped(&h.devices, 32), "browsers": capped(&h.browsers, 64), "params": capped(&h.params, 300),
        "servers": capped(&h.servers, 260), "server_errors": capped(&h.server_errors, 260), "server_latency": capped(&h.server_latency, 270),
        "blocked": capped(&h.blocked, 200), "blocked_countries": capped(&h.blocked_countries, 8), "blocked_paths": capped(&h.blocked_paths, 400),
        "waf_blocked": capped(&h.waf_blocked, 16), "waf_logged": capped(&h.waf_logged, 16),
        "waf_countries": capped(&h.waf_countries, 10), "waf_paths": capped(&h.waf_paths, 400), "waf_visitors": hll(&h.waf_visitors),
    })
}

pub fn port_row(cluster_id: &str, o: &Owner, port: u16, p: &PortTotals) -> Value {
    json!({
        "ts": p.minute, "team_id": o.team_id, "service_id": o.service_id, "port": port,
        "protocol": if p.protocol == "UDP" { "UDP" } else { "TCP" },
        "cluster_id": cluster_id, "project_id": o.project_id,
        "connections": p.connections, "bytes_in": p.bytes_in, "bytes_out": p.bytes_out,
        "duration": hist(&p.duration), "outcomes": capped(&p.outcomes, 16), "countries": capped(&p.countries, 8),
        "visitors": hll(&p.visitors), "blocked": capped(&p.blocked, 200), "blocked_countries": capped(&p.blocked_countries, 8),
    })
}

impl Observability {
    /// Stores a batch of a cluster's totals: its services' hosts and ports
    /// only (Pyo's view of them, not the cluster's), minutes from the
    /// last week. Teams with observability off get none stored, and pages
    /// and query parameter names are kept as each team's query string
    /// setting says (again: an agent that doesn't know it yet sends them
    /// all). Returns the rows stored.
    pub async fn ingest_totals(&self, cluster_id: &str, b: &TotalsBatch) -> ApiResult<usize> {
        let Some(ch) = self.store().await else {
            return Ok(0);
        };
        if !self.is_ready().await {
            return Err(ApiError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, "Observability storage is starting."));
        }
        let now = pyo_proto::now_ms() / 1000;
        let fresh = |minute: i64| minute % 60 == 0 && minute > now - 7 * 86400 && minute < now + 300;
        let owners = self.owners().await?;
        let routes = if b.ports.is_empty() { Default::default() } else { self.port_routes().await? };
        // What applies to each team the batch is about.
        let mut ids: Vec<String> = b
            .hosts
            .iter()
            .take(MAX_ROWS)
            .filter_map(|h| {
                let host = h.host.to_lowercase();
                match h.service_id.as_str() {
                    "" => owners.get(&host),
                    s => owners.get(&crate::observability::path_owner_key(&host, s)),
                }
            })
            .map(|o| o.team_id.clone())
            .chain(b.ports.iter().take(MAX_ROWS).filter_map(|p| routes.get(&(cluster_id.to_owned(), p.key.clone()))).map(|(o, _)| o.team_id.clone()))
            .collect();
        ids.sort();
        ids.dedup();
        let settings = self.teams(&ids).await?;
        let applies = |team: &str| settings.get(team).copied().unwrap_or_default();
        let mut hosts = String::new();
        let mut n = 0;
        let mut teams: Vec<String> = vec![];
        for h in b.hosts.iter().take(MAX_ROWS) {
            let host = h.host.to_lowercase();
            // A path's service, when it has one of the hostname's paths.
            let owner = match h.service_id.as_str() {
                "" => owners.get(&host),
                s => owners.get(&crate::observability::path_owner_key(&host, s)),
            };
            let Some(o) = owner.filter(|o| o.cluster_id == cluster_id) else { continue };
            let team = applies(&o.team_id);
            if !fresh(h.minute) || !team.on {
                continue;
            }
            let owner = Owner { team_id: o.team_id.clone(), project_id: o.project_id.clone(), service_id: o.service_id.clone() };
            if !teams.contains(&owner.team_id) {
                teams.push(owner.team_id.clone());
            }
            let mut h = h.clone();
            h.host = host;
            shape_queries(&mut h, team.query_strings);
            hosts.push_str(&host_row(cluster_id, &owner, &h).to_string());
            hosts.push('\n');
            n += 1;
        }
        let mut ports = String::new();
        for p in b.ports.iter().take(MAX_ROWS) {
            let Some((o, port)) = routes.get(&(cluster_id.to_owned(), p.key.clone())) else { continue };
            if !fresh(p.minute) || !applies(&o.team_id).on {
                continue;
            }
            let owner = Owner { team_id: o.team_id.clone(), project_id: o.project_id.clone(), service_id: o.service_id.clone() };
            ports.push_str(&port_row(cluster_id, &owner, *port, p).to_string());
            ports.push('\n');
            n += 1;
        }
        // Stored once per batch, however often it's sent.
        let token = |table: &str| format!("{cluster_id}:{}:{table}", b.id.chars().take(100).collect::<String>());
        if !hosts.is_empty() {
            ch.insert("traffic_1m", hosts, &token("traffic")).await?;
        }
        if !ports.is_empty() {
            ch.insert("ports_1m", ports, &token("ports")).await?;
        }
        if !b.waf_messages.is_empty() && !teams.is_empty() {
            let mut rows = String::new();
            for team in &teams {
                for (rule, msg) in b.waf_messages.iter().take(500) {
                    if rule.len() <= 16 {
                        let msg: String = msg.chars().take(300).collect();
                        rows.push_str(&json!({ "team_id": team, "rule": rule, "message": msg, "seen": now }).to_string());
                        rows.push('\n');
                    }
                }
            }
            ch.insert("waf_rules", rows, &token("waf")).await?;
        }
        Ok(n)
    }
}

/// A host's minute with its pages and query parameter names as the team's
/// query string setting keeps them.
pub fn shape_queries(h: &mut HostTotals, q: crate::teamobs::QueryStrings) {
    use crate::teamobs::QueryStrings;
    if q == QueryStrings::Keep {
        return;
    }
    if q == QueryStrings::None {
        h.params.clear();
    }
    let mut pages: BTreeMap<String, u64> = BTreeMap::new();
    for (url, n) in std::mem::take(&mut h.pages) {
        *pages.entry(t::shape_url(&url, q.as_str())).or_insert(0) += n;
    }
    h.pages = pages;
}

/// Pyo's view of a cluster's traffic for its traffic store: its services'
/// hosts and proxied ports, their firewalls, and the cluster's team's
/// settings (teamobs.rs) and limits.
pub async fn context(db: &sqlx::PgPool, cluster_id: &str, team_id: &str) -> ApiResult<TrafficContext> {
    let mut tx = db.begin().await?;
    let rows: Vec<(String, String, String, Value, Value)> = sqlx::query_as(
        "SELECT a.id, p.namespace, a.name, a.spec->'domains', a.spec->'ports'
           FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id
          WHERE a.cluster_id = $1 AND NOT a.deleting",
    )
    .bind(cluster_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut ctx = TrafficContext::default();
    // Every path of each hostname, to tell the services apart on one whose
    // paths go to several (paths.rs).
    let mut paths: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (id, ns, name, domains, ports) in rows {
        for d in domains.as_array().into_iter().flatten() {
            if let Some(h) = d["hostname"].as_str() {
                let h = h.to_lowercase();
                // The hostname's own: its "/" route, else the entry that holds it.
                if crate::paths::path_of(d) == "/" || (crate::paths::holds(d) && !ctx.hosts.contains_key(&h)) {
                    ctx.hosts.insert(h.clone(), id.clone());
                }
                paths.entry(h).or_default().push((crate::paths::path_of(d).to_owned(), id.clone()));
            }
        }
        for (key, port) in port_keys(&ns, &name, &ports) {
            ctx.ports.insert(key, (id.clone(), port));
        }
        let f = crate::firewall::for_service(&mut tx, &id).await?;
        if !f.is_unrestricted() {
            ctx.firewalls.insert(id, f);
        }
    }
    for (host, mut ps) in paths {
        let mut services: Vec<&String> = ps.iter().map(|(_, s)| s).collect();
        services.sort();
        services.dedup();
        if services.len() > 1 {
            ps.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
            ctx.paths.insert(host, ps);
        }
    }
    let prefs: Value = sqlx::query_scalar("SELECT preferences FROM pyo.teams WHERE id = $1")
        .bind(team_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or_default();
    tx.commit().await?;
    let (mb, days) = t::limits_from(&prefs);
    ctx.max_mb = mb;
    ctx.max_days = days;
    let team = crate::teamobs::for_team(db, team_id).await?;
    ctx.ip_mode = crate::teamobs::ip_str(team.ip_mode).to_owned();
    ctx.query_strings = team.query_strings.as_str().to_owned();
    // Off, the store is removed; on the cluster Pyo runs on it stays (it
    // runs with ClickHouse) and keeps nothing either.
    ctx.request_logs_off = !team.request_logs || !team.on;
    Ok(ctx)
}

/// A service's proxied ports as the cluster names them: its route
/// (`<namespace>-<service>-<protocol>-<port>`) and where the proxy listens
/// (`<protocol>/<listen port>`), each with the public port.
pub fn port_keys(ns: &str, name: &str, ports: &Value) -> Vec<(String, u16)> {
    let mut out = vec![];
    for p in ports.as_array().into_iter().flatten().filter_map(crate::deploy::port_spec).filter(|p| p.proxied()) {
        let public = p.public() as u16;
        out.push((format!("{ns}-{name}-{}-{}", p.transport().to_lowercase(), p.public()), public));
        out.push((format!("{}/{}", p.transport(), pyo_proto::proxy_listen_port(p.public())), public));
    }
    out
}

// ── Reading totals ──────────────────────────────────────────────────────────

/// Which table a range reads, its buckets, and the window: (table suffix,
/// points, step, start, end). The last bucket holds `now`.
pub fn window(range: &str, now: i64) -> (&'static str, i64, i64, i64, i64) {
    let (table, points, step) = match range {
        "1h" => ("1m", 60, 60),
        "24h" => ("1m", 48, 1800),
        "30d" => ("1h", 30, 86400),
        _ => ("1h", 42, 14400),
    };
    let end = (now / step + 1) * step;
    (table, points, step, end - points * step, end)
}

/// The request summary's buckets, in seconds: minutes for ranges up to two
/// days, else hours (the tables kept that long).
fn summary_step(now_s: i64, since_s: i64) -> i64 {
    if now_s - since_s <= 2 * 86400 { 60 } else { 3600 }
}

/// Which time the request summary covers (ms): from the start of the bucket
/// `since_s` is in, to the end of the newest bucket stored (`last_s`, its
/// start; 0 when none is). A bucket is stored once its minute is over and
/// the cluster has sent it, so the newest minute or two of requests come
/// after `to`, which is null while nothing is counted.
fn summary_window(since_s: i64, step: i64, last_s: i64) -> (i64, Option<i64>) {
    let from = since_s / step * step;
    let to = (last_s >= from && last_s > 0).then(|| (last_s + step) * 1000);
    (from * 1000, to)
}

fn num(v: &Value, k: &str) -> f64 {
    v[k].as_f64()
        .or_else(|| v[k].as_str().and_then(|s| s.parse().ok()))
        .filter(|x| x.is_finite())
        .unwrap_or(0.0)
}

/// A map column as returned (an object) as (key, count) pairs, largest first.
fn pairs(v: &Value) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = v
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, n)| (k.clone(), n.as_f64().or_else(|| n.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0)))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// A latency map column as a histogram.
fn histogram(v: &Value) -> BTreeMap<u16, u64> {
    v.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, n)| Some((k.parse().ok()?, n.as_u64().or_else(|| n.as_str().and_then(|s| s.parse().ok()))?)))
        .collect()
}

fn q(h: &BTreeMap<u16, u64>, q: f64) -> f64 {
    latency_quantile(h, q).round()
}

/// `[a,b]` as a ClickHouse array literal parameter.
pub fn array_param(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|h| format!("'{}'", h.replace(['\\', '\''], "")))
            .collect::<Vec<_>>()
            .join(",")
    )
}

impl Observability {
    /// The analytics page's data for a set of hostnames of one team, from
    /// the totals.
    pub async fn query(&self, team_id: &str, hosts: &[String], services: &[String], range: &str) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let (table, points, step, start, end) = window(range, pyo_proto::now_ms() / 1000);
        let prev_start = start - points * step;
        let hosts_param = array_param(hosts);
        let p = |s: i64, e: i64| -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("hosts", hosts_param.clone()),
                ("services", array_param(services)),
                ("start", s.to_string()),
                ("end", e.to_string()),
                ("step", step.to_string()),
            ]
        };
        let from = format!("pyo_observability.traffic_{table}");
        const W: &str = "team_id = {team:String} AND has({hosts:Array(String)}, host) AND (empty({services:Array(String)}) OR has({services:Array(String)}, service_id)) AND ts >= toDateTime({start:Int64}) AND ts < toDateTime({end:Int64})";
        let visitors = hll_sql("maxMap(visitors)");
        let summary_sql = format!(
            "SELECT sum(requests) AS requests, sum(pageviews) AS pageviews, sum(bytes) / 1e6 AS mb, sum(errors) AS errors,
                    sumMap(latency) AS latency, sumMap(statuses) AS statuses, {visitors} AS visitors
               FROM {from} WHERE {W}"
        );
        let top = |col: &str, limit: u32| {
            format!(
                "SELECT k, sum(v) AS n FROM {from} ARRAY JOIN mapKeys({col}) AS k, mapValues({col}) AS v WHERE {W} GROUP BY k ORDER BY n DESC, k LIMIT {limit}"
            )
        };
        let (cur, prev, series, paths, pages, refs, countries, devices, browsers, params) = tokio::try_join!(
            ch.rows(summary_sql.clone(), p(start, end)),
            ch.rows(summary_sql.clone(), p(prev_start, start)),
            ch.rows(
                format!(
                    "SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, sum(requests) AS requests,
                            sum(pageviews) AS pageviews, sum(bytes) / 1e6 AS mb, sum(errors) AS errors, sumMap(latency) AS latency, {visitors} AS visitors
                       FROM {from} WHERE {W} GROUP BY b ORDER BY b"
                ),
                p(start, end)
            ),
            ch.rows(top("paths", 25), p(start, end)),
            ch.rows(top("pages", 100), p(start, end)),
            ch.rows(top("referrers", 25), p(start, end)),
            ch.rows(top("countries", 25), p(start, end)),
            ch.rows(top("devices", 10), p(start, end)),
            ch.rows(top("browsers", 15), p(start, end)),
            ch.rows(top("params", 30), p(start, end)),
        )?;
        let c = cur.first().cloned().unwrap_or_default();
        let pv = prev.first().cloned().unwrap_or_default();
        let requests = num(&c, "requests");
        let lat = histogram(&c["latency"]);
        let by_bucket: HashMap<i64, &Value> = series.iter().map(|r| (r["b"].as_i64().unwrap_or(0), r)).collect();
        let series: Vec<Value> = (0..points)
            .map(|i| {
                let b = start + i * step;
                let r = by_bucket.get(&b);
                let g = |k: &str| r.map(|r| num(r, k)).unwrap_or(0.0);
                let h = r.map(|r| histogram(&r["latency"])).unwrap_or_default();
                json!({
                    "at": b * 1000,
                    "requests": g("requests"), "pageviews": g("pageviews"), "visitors": g("visitors").round(), "bandwidthMb": g("mb"), "errors": g("errors"),
                    "p50": q(&h, 0.5), "p95": q(&h, 0.95), "p99": q(&h, 0.99),
                })
            })
            .collect();
        let statuses = pairs(&c["statuses"]);
        let status_codes: Vec<Value> = ["2xx", "3xx", "4xx", "5xx"]
            .iter()
            .map(|code| json!({ "code": code, "count": statuses.iter().find(|s| s.0 == *code).map(|s| s.1).unwrap_or(0.0) }))
            .collect();
        let kv = |rows: &[Value], key: &str, label: &str| -> Vec<Value> {
            rows.iter().map(|r| json!({ key: r["k"], label: num(r, "n") })).collect()
        };
        let path_of = |url: &str| url.split('?').next().unwrap_or(url).to_owned();
        // Pages and parameter names as the team keeps them now, also those
        // stored before it chose.
        let qs = self.team(team_id).await.query_strings;
        let mut shown: Vec<(String, f64)> = vec![];
        for r in &pages {
            let url = t::shape_url(r["k"].as_str().unwrap_or(""), qs.as_str());
            match shown.iter_mut().find(|p| p.0 == url) {
                Some(p) => p.1 += num(r, "n"),
                None => shown.push((url, num(r, "n"))),
            }
        }
        shown.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let params = if qs == crate::teamobs::QueryStrings::None { vec![] } else { params };
        let summary = json!({
            "range": range,
            "requests": requests,
            "estimatedVisitors": num(&c, "visitors").round(),
            "pageviews": num(&c, "pageviews"),
            "bandwidthMb": num(&c, "mb"),
            "errorRate": if requests > 0.0 { num(&c, "errors") / requests } else { 0.0 },
            "latency": { "p50": q(&lat, 0.5), "p95": q(&lat, 0.95), "p99": q(&lat, 0.99) },
            "statusCodes": status_codes,
            "topPaths": kv(&paths, "path", "requests"),
            "referrers": kv(&refs, "source", "views"),
            "countries": countries.iter().map(|r| json!({ "code": r["k"].as_str().filter(|c| !c.is_empty()).unwrap_or("??"), "requests": num(r, "n") })).collect::<Vec<_>>(),
            "pages": shown.iter().map(|(url, n)| json!({ "url": url, "path": path_of(url), "views": n })).collect::<Vec<_>>(),
            "devices": kv(&devices, "label", "views"),
            "browsers": kv(&browsers, "label", "views"),
            "queryParams": params.iter().map(|r| json!({ "key": r["k"], "requests": num(r, "n"), "redacted": t::sensitive(r["k"].as_str().unwrap_or("")) })).collect::<Vec<_>>(),
        });
        let prev_requests = num(&pv, "requests");
        Ok(json!({
            "summary": summary,
            "series": series,
            "previous": {
                "requests": prev_requests,
                "estimatedVisitors": num(&pv, "visitors").round(),
                "pageviews": num(&pv, "pageviews"),
                "errorRate": if prev_requests > 0.0 { num(&pv, "errors") / prev_requests } else { 0.0 },
            },
        }))
    }

    /// Requests per hostname over a range, each with its series (the
    /// domains list): `range`, `step` (seconds per point), `start` (ms, the
    /// first point) and `domains`. One query for all of them. No domains
    /// while observability is off or not set up.
    pub async fn domain_traffic(&self, team_id: &str, hosts: &[String], range: &str) -> ApiResult<Value> {
        let (table, points, step, start, end) = window(range, pyo_proto::now_ms() / 1000);
        let out = |domains: Vec<Value>| json!({ "range": range, "step": step, "start": start * 1000, "domains": domains });
        let Some(ch) = self.store().await else { return Ok(out(vec![])) };
        if hosts.is_empty() {
            return Ok(out(vec![]));
        }
        let rows = ch
            .rows(
                format!(
                    "SELECT host, toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, sum(requests) AS n
                       FROM pyo_observability.traffic_{table}
                      WHERE team_id = {{team:String}} AND has({{hosts:Array(String)}}, host)
                        AND ts >= toDateTime({{start:Int64}}) AND ts < toDateTime({{end:Int64}})
                      GROUP BY host, b"
                ),
                vec![
                    ("team", team_id.to_owned()),
                    ("hosts", array_param(hosts)),
                    ("start", start.to_string()),
                    ("end", end.to_string()),
                    ("step", step.to_string()),
                ],
            )
            .await?;
        Ok(out(per_host(&rows, hosts, start, step, points)))
    }

    /// The request explorer's totals for some hosts over a range: requests,
    /// bytes, errors and response times. None without storage.
    ///
    /// They're added up per minute (an hour for ranges over two days), and
    /// a cluster sends a minute once it's over ([`summary_window`]): `from`
    /// and `to` (ms) say which whole minutes are counted, `to` null while
    /// none has come. The newest requests in the log come after `to`.
    pub async fn request_summary(&self, team_id: &str, hosts: &[String], services: &[String], since_s: i64) -> ApiResult<Option<Value>> {
        let Some(ch) = self.store().await else { return Ok(None) };
        let step = summary_step(pyo_proto::now_ms() / 1000, since_s);
        let table = if step == 60 { "1m" } else { "1h" };
        let from = since_s / step * step;
        let r = ch
            .rows(
                format!(
                    "SELECT sum(requests) AS requests, sum(bytes) AS bytes, sumMap(statuses) AS statuses, sumMap(latency) AS latency,
                            toUnixTimestamp(max(ts)) AS last
                       FROM pyo_observability.traffic_{table}
                      WHERE team_id = {{team:String}} AND has({{hosts:Array(String)}}, host)
                        AND (empty({{services:Array(String)}}) OR has({{services:Array(String)}}, service_id)) AND ts >= toDateTime({{from:Int64}})"
                ),
                vec![("team", team_id.to_owned()), ("hosts", array_param(hosts)), ("services", array_param(services)), ("from", from.to_string())],
            )
            .await?;
        let r = r.first().cloned().unwrap_or_default();
        let s = pairs(&r["statuses"]);
        let count = |c: &str| s.iter().find(|x| x.0 == c).map(|x| x.1).unwrap_or(0.0);
        let h = histogram(&r["latency"]);
        let (from_ms, to_ms) = summary_window(since_s, step, num(&r, "last") as i64);
        Ok(Some(json!({
            "requests": num(&r, "requests"), "bytes": num(&r, "bytes"),
            "serverErrors": count("5xx"), "clientErrors": count("4xx"),
            "p50": q(&h, 0.5), "p95": q(&h, 0.95),
            "from": from_ms, "to": to_ms,
        })))
    }

    /// Requests, server errors and response times per answering server,
    /// for some hostnames over the last `hours`. Empty without storage.
    pub async fn per_server(&self, team_id: &str, hosts: &[String], services: &[String], hours: i64) -> ApiResult<Vec<Value>> {
        let Some(ch) = self.store().await else { return Ok(vec![]) };
        if hosts.is_empty() {
            return Ok(vec![]);
        }
        let since = pyo_proto::now_ms() / 1000 - hours * 3600;
        let r = ch
            .rows(
                "SELECT sumMap(servers) AS servers, sumMap(server_errors) AS errors, sumMap(server_latency) AS latency
                   FROM pyo_observability.traffic_1h
                  WHERE team_id = {team:String} AND has({hosts:Array(String)}, host) AND (empty({services:Array(String)}) OR has({services:Array(String)}, service_id)) AND ts >= toStartOfHour(toDateTime({since:Int64}))"
                    .into(),
                vec![("team", team_id.to_owned()), ("hosts", array_param(hosts)), ("services", array_param(services)), ("since", since.to_string())],
            )
            .await?;
        let r = r.first().cloned().unwrap_or_default();
        let errors: HashMap<String, f64> = pairs(&r["errors"]).into_iter().collect();
        // "<server>|<bucket>" -> requests, per server.
        let mut hists: HashMap<String, BTreeMap<u16, u64>> = HashMap::new();
        for (k, n) in pairs(&r["latency"]) {
            if let Some((server, b)) = k.rsplit_once('|')
                && let Ok(b) = b.parse::<u16>()
            {
                *hists.entry(server.to_owned()).or_default().entry(b).or_insert(0) += n as u64;
            }
        }
        Ok(pairs(&r["servers"])
            .into_iter()
            .map(|(node, requests)| {
                let h = hists.get(&node).cloned().unwrap_or_default();
                let total: u64 = h.values().sum();
                let avg = if total > 0 {
                    h.iter().map(|(b, n)| t::latency_of(*b) * *n as f64).sum::<f64>() / total as f64
                } else {
                    0.0
                };
                json!({ "node": node, "requests": requests, "errors": errors.get(&node).copied().unwrap_or(0.0),
                        "avgMs": (avg * 10.0).round() / 10.0, "p95Ms": q(&h, 0.95) })
            })
            .collect())
    }

    /// Requests per answering server per hour, for the last `hours`
    /// (sparklines). Empty without storage.
    pub async fn per_server_hourly(&self, team_id: &str, hosts: &[String], services: &[String], hours: i64) -> ApiResult<Vec<Value>> {
        let Some(ch) = self.store().await else { return Ok(vec![]) };
        if hosts.is_empty() {
            return Ok(vec![]);
        }
        let since = pyo_proto::now_ms() / 1000 - hours * 3600;
        let rows = ch
            .rows(
                "SELECT toUnixTimestamp(ts) AS hour, sumMap(servers) AS servers
                   FROM pyo_observability.traffic_1h
                  WHERE team_id = {team:String} AND has({hosts:Array(String)}, host) AND (empty({services:Array(String)}) OR has({services:Array(String)}, service_id)) AND ts >= toStartOfHour(toDateTime({since:Int64}))
                  GROUP BY hour"
                    .into(),
                vec![("team", team_id.to_owned()), ("hosts", array_param(hosts)), ("services", array_param(services)), ("since", since.to_string())],
            )
            .await?;
        Ok(rows
            .iter()
            .flat_map(|r| {
                let hour = r["hour"].clone();
                pairs(&r["servers"]).into_iter().map(move |(node, n)| json!({ "node": node, "hour": hour, "requests": n }))
            })
            .collect())
    }

    /// A service's connections to its public TCP and UDP ports through the
    /// gateway: per port, over time, by country and outcome. The latest
    /// ones are the cluster's (`recent`, added by the caller).
    pub async fn connections(&self, team_id: &str, service_id: &str, range: &str) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let (table, points, step, start, end) = bar_window(range);
        let p = || -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("service", service_id.to_owned()),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("step", step.to_string()),
            ]
        };
        let from = format!("pyo_observability.ports_{table}");
        const W: &str = "team_id = {team:String} AND service_id = {service:String} AND ts >= toDateTime({start:Int64}) AND ts < toDateTime({end:Int64})";
        let clients = hll_sql("maxMap(visitors)");
        let (ports, series, countries, outcomes) = tokio::try_join!(
            ch.rows(format!("SELECT port, protocol, sum(connections) AS c, {clients} AS v, sum(bytes_in) AS bin, sum(bytes_out) AS bout, sumMap(duration) AS d, sumMap(outcomes) AS o FROM {from} WHERE {W} GROUP BY port, protocol ORDER BY port"), p()),
            ch.rows(format!("SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, sum(connections) AS c, {clients} AS v, sum(bytes_in) + sum(bytes_out) AS bytes FROM {from} WHERE {W} GROUP BY b ORDER BY b"), p()),
            ch.rows(format!("SELECT k, sum(v) AS n FROM {from} ARRAY JOIN mapKeys(countries) AS k, mapValues(countries) AS v WHERE {W} GROUP BY k ORDER BY n DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT k, sum(v) AS n FROM {from} ARRAY JOIN mapKeys(outcomes) AS k, mapValues(outcomes) AS v WHERE {W} GROUP BY k"), p()),
        )?;
        let by_bucket: HashMap<i64, &Value> = series.iter().map(|r| (r["b"].as_i64().unwrap_or(0), r)).collect();
        let series: Vec<Value> = (0..points)
            .map(|i| {
                let b = start + i * step;
                let r = by_bucket.get(&b);
                let g = |k: &str| r.map(|r| num(r, k)).unwrap_or(0.0);
                json!({ "at": b * 1000, "connections": g("c"), "clients": g("v").round(), "bytes": g("bytes") })
            })
            .collect();
        Ok(json!({
            "range": range,
            "ports": ports.iter().map(|r| {
                let d = histogram(&r["d"]);
                let o: HashMap<String, f64> = pairs(&r["o"]).into_iter().collect();
                let out = |k: &str| o.get(k).copied().unwrap_or(0.0);
                json!({
                    "port": r["port"], "protocol": r["protocol"], "connections": num(r, "c"), "clients": num(r, "v").round(),
                    "bytesIn": num(r, "bin"), "bytesOut": num(r, "bout"), "durationP50Ms": q(&d, 0.5), "durationP95Ms": q(&d, 0.95),
                    "blocked": out("blocked"), "limited": out("limited"), "unreachable": out("unreachable"),
                })
            }).collect::<Vec<_>>(),
            "series": series,
            "countries": countries.iter().map(|r| json!({ "code": r["k"].as_str().filter(|c| !c.is_empty()).unwrap_or("??"), "connections": num(r, "n") })).collect::<Vec<_>>(),
            "outcomes": outcomes.iter().map(|r| json!({ "outcome": r["k"], "count": num(r, "n") })).collect::<Vec<_>>(),
            "recent": [],
        }))
    }

    /// What the firewall turned away for these services: over time, per
    /// rule, country and website path. Sources and the latest are the
    /// cluster's (added by the caller).
    pub async fn blocked(&self, team_id: &str, services: &[String], range: &str) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let (table, _, step, start, end) = bar_window(range);
        let p = || -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("services", array_param(services)),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("step", step.to_string()),
            ]
        };
        const W: &str = "team_id = {team:String} AND has({services:Array(String)}, service_id) AND ts >= toDateTime({start:Int64}) AND ts < toDateTime({end:Int64})";
        // Website requests and port connections, as one.
        let both = |col: &str| {
            format!(
                "(SELECT ts, {col} AS m FROM pyo_observability.traffic_{table} WHERE {W} UNION ALL SELECT ts, {col} AS m FROM pyo_observability.ports_{table} WHERE {W})"
            )
        };
        let (rules, countries, paths, series) = tokio::try_join!(
            ch.rows(format!("SELECT k, sum(v) AS n FROM {} ARRAY JOIN mapKeys(m) AS k, mapValues(m) AS v GROUP BY k ORDER BY n DESC LIMIT 50", both("blocked")), p()),
            ch.rows(format!("SELECT k, sum(v) AS n FROM {} ARRAY JOIN mapKeys(m) AS k, mapValues(m) AS v GROUP BY k ORDER BY n DESC LIMIT 25", both("blocked_countries")), p()),
            ch.rows(format!("SELECT k, sum(v) AS n FROM pyo_observability.traffic_{table} ARRAY JOIN mapKeys(blocked_paths) AS k, mapValues(blocked_paths) AS v WHERE {W} GROUP BY k ORDER BY n DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, sum(arraySum(mapValues(m))) AS c FROM {} GROUP BY b", both("blocked")), p()),
        )?;
        Ok(json!({
            "range": range,
            "total": rules.iter().map(|r| num(r, "n")).sum::<f64>(),
            "series": crate::observability::bar_series(&series, range, |r| json!({ "count": r.map(|r| num(r, "c")).unwrap_or(0.0) })),
            "rules": rules.iter().map(|r| json!({ "rule": r["k"], "count": num(r, "n") })).collect::<Vec<_>>(),
            "countries": countries.iter().map(|r| json!({ "country": r["k"], "count": num(r, "n") })).collect::<Vec<_>>(),
            "paths": paths.iter().map(|r| json!({ "path": r["k"], "count": num(r, "n") })).collect::<Vec<_>>(),
            "sources": [],
            "recent": [],
        }))
    }

    /// The WAF's findings for these services: blocked and logged requests
    /// over time, per rule, domain, country and path, and the custom
    /// rules' match counts. Sources and the latest are the cluster's.
    pub async fn waf_hits(&self, team_id: &str, services: &[String], range: &str) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let (table, _, step, start, end) = bar_window(range);
        let p = || -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("services", array_param(services)),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("step", step.to_string()),
            ]
        };
        let from = format!("pyo_observability.traffic_{table}");
        const W: &str = "team_id = {team:String} AND has({services:Array(String)}, service_id) AND ts >= toDateTime({start:Int64}) AND ts < toDateTime({end:Int64})";
        let clients = hll_sql("maxMap(waf_visitors)");
        let split = |col: &str, limit: u32| {
            format!(
                "SELECT substring(k, 3) AS key, sumIf(v, startsWith(k, 'b:')) AS nb, sumIf(v, startsWith(k, 'l:')) AS nl
                   FROM {from} ARRAY JOIN mapKeys({col}) AS k, mapValues({col}) AS v WHERE {W} GROUP BY key ORDER BY nb + nl DESC LIMIT {limit}"
            )
        };
        let (totals, blocked, logged, hosts, series, countries, paths, messages) = tokio::try_join!(
            ch.rows(format!("SELECT sum(arraySum(mapValues(waf_blocked))) AS nb, sum(arraySum(mapValues(waf_logged))) AS nl, {clients} AS v FROM {from} WHERE {W}"), p()),
            ch.rows(format!("SELECT k, sum(v) AS n FROM {from} ARRAY JOIN mapKeys(waf_blocked) AS k, mapValues(waf_blocked) AS v WHERE {W} GROUP BY k"), p()),
            ch.rows(format!("SELECT k, sum(v) AS n FROM {from} ARRAY JOIN mapKeys(waf_logged) AS k, mapValues(waf_logged) AS v WHERE {W} GROUP BY k"), p()),
            ch.rows(format!("SELECT host, sum(arraySum(mapValues(waf_blocked))) AS nb, sum(arraySum(mapValues(waf_logged))) AS nl FROM {from} WHERE {W} GROUP BY host HAVING nb + nl > 0 ORDER BY nb + nl DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, sum(arraySum(mapValues(waf_blocked))) AS nb, sum(arraySum(mapValues(waf_logged))) AS nl FROM {from} WHERE {W} GROUP BY b"), p()),
            ch.rows(split("waf_countries", 25), p()),
            ch.rows(split("waf_paths", 25), p()),
            ch.rows("SELECT rule, argMax(message, seen) AS message FROM pyo_observability.waf_rules WHERE team_id = {team:String} GROUP BY rule".into(), vec![("team", team_id.to_owned())]),
        )?;
        let t = totals.first().cloned().unwrap_or_default();
        let msg: HashMap<String, String> = messages
            .iter()
            .map(|r| (r["rule"].as_str().unwrap_or("").to_owned(), r["message"].as_str().unwrap_or("").to_owned()))
            .collect();
        // Every rule's matches, blocked or only logged.
        let mut by_rule: BTreeMap<u64, f64> = BTreeMap::new();
        for r in blocked.iter().chain(logged.iter()) {
            if let Ok(rule) = r["k"].as_str().unwrap_or("").parse::<u64>() {
                *by_rule.entry(rule).or_insert(0.0) += num(r, "n");
            }
        }
        // Custom rules are numbered from 1000000 (pyo_proto::WAF_RULE_IDS);
        // the Core Rule Set's from 949000 are its scoring, not findings.
        let mut rules: Vec<(u64, f64)> = by_rule
            .iter()
            .filter(|(r, _)| **r < 949_000 || **r >= 1_000_000)
            .map(|(r, n)| (*r, *n))
            .collect();
        rules.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        rules.truncate(25);
        Ok(json!({
            "range": range,
            "blocked": num(&t, "nb"), "logged": num(&t, "nl"), "clients": num(&t, "v").round(),
            "rules": rules.iter().map(|(r, n)| json!({ "rule": r, "message": msg.get(&r.to_string()).cloned().unwrap_or_default(), "count": n })).collect::<Vec<_>>(),
            "hosts": hosts.iter().map(|r| json!({ "host": r["host"], "blocked": num(r, "nb"), "logged": num(r, "nl") })).collect::<Vec<_>>(),
            "series": crate::observability::bar_series(&series, range, |r| json!({ "blocked": r.map(|r| num(r, "nb")).unwrap_or(0.0), "logged": r.map(|r| num(r, "nl")).unwrap_or(0.0) })),
            "countries": countries.iter().map(|r| json!({ "country": r["key"], "blocked": num(r, "nb"), "logged": num(r, "nl") })).collect::<Vec<_>>(),
            "paths": paths.iter().map(|r| json!({ "path": r["key"], "blocked": num(r, "nb"), "logged": num(r, "nl") })).collect::<Vec<_>>(),
            "custom": by_rule.iter().filter(|(r, _)| **r >= 1_000_000).map(|(r, n)| json!({ "rule": r, "count": n })).collect::<Vec<_>>(),
            "sources": [],
            "recent": [],
        }))
    }
}

/// Rows of (host, bucket `b`, requests `n`) as one entry per host asked
/// for that had requests: its total and one point per bucket (0 where
/// none came), busiest first.
fn per_host(rows: &[Value], hosts: &[String], start: i64, step: i64, points: i64) -> Vec<Value> {
    let mut by_host: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for r in rows {
        let host = r["host"].as_str().unwrap_or("");
        let b = r["b"].as_i64().or_else(|| r["b"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
        let i = (b - start).div_euclid(step);
        if !hosts.iter().any(|h| h == host) || !(0..points).contains(&i) {
            continue;
        }
        by_host.entry(host).or_insert_with(|| vec![0.0; points as usize])[i as usize] += num(r, "n");
    }
    let mut out: Vec<(f64, Value)> = by_host
        .into_iter()
        .map(|(host, series)| {
            let total: f64 = series.iter().sum();
            (total, json!({ "host": host, "requests": total, "series": series }))
        })
        .filter(|(total, _)| *total > 0.0)
        .collect();
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out.into_iter().map(|(_, v)| v).collect()
}

/// The firewall's, WAF's and connections' windows: like `window`, ending
/// now (`bar_steps`).
fn bar_window(range: &str) -> (&'static str, i64, i64, i64, i64) {
    let (points, step) = bar_steps(range);
    let table = if step < 3600 { "1m" } else { "1h" };
    let end = (pyo_proto::now_ms() / 1000 / step + 1) * step;
    (table, points, step, end - points * step, end)
}

impl ClickHouse {
    /// Inserts JSON rows into a table, stored once per `token`.
    pub async fn insert(&self, table: &str, rows: String, token: &str) -> anyhow::Result<()> {
        self.exec_with(
            &format!("INSERT INTO pyo_observability.{table} FORMAT JSONEachRow"),
            Some(rows),
            &[],
            &[("insert_deduplication_token", token.to_owned()), ("deduplicate_blocks_in_dependent_materialized_views", "1".into())],
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Totals from an agent that doesn't know the team's query string
    /// setting yet are stored as the setting says.
    #[test]
    fn pages_and_parameters_as_the_team_keeps_them() {
        use crate::teamobs::QueryStrings;
        let mut h = HostTotals::default();
        h.pages.insert("/a?q=shoes&page=2".into(), 3);
        h.pages.insert("/a?q=hats".into(), 2);
        h.pages.insert("/b".into(), 1);
        h.params.insert("q".into(), 5);
        let mut keep = h.clone();
        shape_queries(&mut keep, QueryStrings::Keep);
        assert_eq!(keep, h);
        let mut names = h.clone();
        shape_queries(&mut names, QueryStrings::Names);
        assert_eq!(names.pages, BTreeMap::from([("/a?q&page".into(), 3), ("/a?q".into(), 2), ("/b".into(), 1)]));
        assert_eq!(names.params, h.params, "names are kept");
        let mut none = h.clone();
        shape_queries(&mut none, QueryStrings::None);
        assert_eq!(none.pages, BTreeMap::from([("/a".into(), 5), ("/b".into(), 1)]));
        assert!(none.params.is_empty());
    }

    #[test]
    fn hourly_views_merge_every_column() {
        let s = schema();
        assert_eq!(s.len(), 7);
        let view = &s[2];
        assert!(view.contains("sumMap(statuses) AS statuses"));
        assert!(view.contains("maxMap(visitors) AS visitors"));
        assert!(view.contains("sum(requests) AS requests"));
        assert!(view.contains("anyLast(cluster_id) AS cluster_id"));
        // Every aggregate column is in the view, and nothing else but the keys.
        assert_eq!(merges(TRAFFIC_COLUMNS).len(), 29);
        assert_eq!(merges(PORT_COLUMNS).len(), 11);
        assert!(s[0].contains("TTL ts + INTERVAL 3 DAY") && s[1].contains("TTL ts + INTERVAL 12 MONTH"));
    }

    #[test]
    fn windows_pick_their_table() {
        let now = 1_794_737_009;
        for (range, table) in [("1h", "1m"), ("24h", "1m"), ("7d", "1h"), ("30d", "1h")] {
            let (tb, points, step, start, end) = window(range, now);
            assert_eq!(tb, table);
            assert_eq!(end - start, points * step);
            assert!(start <= now && now < end && end - step <= now);
        }
    }

    #[test]
    fn the_request_summary_says_which_minutes_it_counts() {
        let now = 1_700_000_000;
        // A day: minutes, from the start of the minute the range starts in.
        let since = now - 86_400;
        assert_eq!(summary_step(now, since), 60);
        let (from, to) = summary_window(since, 60, (now / 60 - 2) * 60);
        assert_eq!(from, since / 60 * 60 * 1000);
        assert!(from <= since * 1000, "every request in the log's range is in it");
        // Up to the end of the newest minute stored: the last minute or two
        // of the log come after it.
        assert_eq!(to, Some((now / 60 - 1) * 60 * 1000));
        assert!(to.unwrap() < now * 1000);
        // Nothing stored yet (requests only just began): no end to show.
        assert_eq!(summary_window(since, 60, 0).1, None);
        // A week: hours.
        let since = now - 7 * 86_400;
        assert_eq!(summary_step(now, since), 3600);
        let (from, to) = summary_window(since, 3600, now / 3600 * 3600);
        assert_eq!(from, since / 3600 * 3600 * 1000);
        assert_eq!(to, Some((now / 3600 + 1) * 3600 * 1000));
    }

    #[test]
    fn rows_keep_to_their_limits() {
        let o = Owner { team_id: "t".into(), project_id: "p".into(), service_id: "s".into() };
        let mut h = HostTotals { minute: 60, host: "a.example".into(), requests: 2, pageviews: 9, errors: 1, ..Default::default() };
        for i in 0..400 {
            h.paths.insert(format!("/{i}"), 1);
        }
        h.paths.insert("/".repeat(1000), 1);
        h.visitors.insert(5, 3);
        h.visitors.insert(9999, 3);
        h.latency.insert(500, 1);
        let r = host_row("c", &o, &h);
        assert_eq!(r["pageviews"], 2, "never more page views than requests");
        assert_eq!(r["paths"].as_object().unwrap().len(), MAX_KEYS);
        assert_eq!(r["visitors"], json!({ "5": 3 }));
        assert_eq!(r["latency"], json!({}));
    }

    #[test]
    fn domain_traffic_keeps_to_the_hosts_and_the_window() {
        let (start, step) = (1_800_000, 1800);
        let hosts = vec!["a.example.com".to_owned(), "b.example.com".to_owned(), "quiet.example.com".to_owned()];
        let rows = [
            json!({ "host": "a.example.com", "b": start, "n": "3" }),
            json!({ "host": "a.example.com", "b": start + 2 * step, "n": 4 }),
            json!({ "host": "b.example.com", "b": (start + step).to_string(), "n": 40 }),
            // Not asked for (another team's, or a host it no longer has).
            json!({ "host": "other.example.net", "b": start, "n": 9 }),
            // Outside the window.
            json!({ "host": "a.example.com", "b": start - step, "n": 100 }),
            json!({ "host": "a.example.com", "b": start + 3 * step, "n": 100 }),
            json!({ "host": "quiet.example.com", "b": start, "n": 0 }),
        ];
        let out = per_host(&rows, &hosts, start, step, 3);
        assert_eq!(
            out,
            [
                json!({ "host": "b.example.com", "requests": 40.0, "series": [0.0, 40.0, 0.0] }),
                json!({ "host": "a.example.com", "requests": 7.0, "series": [3.0, 0.0, 4.0] }),
            ],
            "busiest first, no host without requests"
        );
    }

    #[test]
    fn port_keys_are_the_clusters() {
        let ports = json!([{ "name": "db", "containerPort": 5432, "protocol": "TCP", "exposure": "loadbalancer", "publicPort": 5432 },
                           { "name": "web", "containerPort": 80, "protocol": "TCP", "exposure": "public" }]);
        let keys = port_keys("shop", "db", &ports);
        assert_eq!(keys, [("shop-db-tcp-5432".to_owned(), 5432), ("TCP/5432".to_owned(), 5432)]);
    }

    /// Against a throwaway ClickHouse (`PYO_TEST_CLICKHOUSE_URL`, user
    /// `default`, password in `PYO_TEST_CLICKHOUSE_PASSWORD`): minutes go in
    /// once however often they're sent, add up into hours, and every page
    /// reads them back.
    #[tokio::test]
    #[ignore = "needs a throwaway ClickHouse"]
    async fn clickhouse_end_to_end() {
        let Ok(url) = std::env::var("PYO_TEST_CLICKHOUSE_URL") else { return };
        let password = std::env::var("PYO_TEST_CLICKHOUSE_PASSWORD").unwrap_or_default();
        let ch = ClickHouse::new(url.clone(), "default".into(), password.clone());
        ch.exec("DROP DATABASE IF EXISTS pyo_observability", None, &[]).await.unwrap();
        ch.exec("CREATE DATABASE pyo_observability", None, &[]).await.unwrap();
        for sql in schema() {
            ch.exec(&sql, None, &[]).await.unwrap();
        }
        let db = sqlx::postgres::PgPoolOptions::new().acquire_timeout(std::time::Duration::from_millis(200)).connect_lazy("postgres://nobody@127.0.0.1:1/none").unwrap();
        let obs = Observability::with(db, Some(ClickHouse::new(url, "default".into(), password)));
        let o = Owner { team_id: "team_1".into(), project_id: "prj_1".into(), service_id: "svc_1".into() };
        let now = pyo_proto::now_ms() / 1000 / 60 * 60;
        let minute = |m: i64, host: &str, visitors: std::ops::Range<u64>| {
            let mut h = HostTotals { minute: now - m * 60, host: host.into(), requests: 10, pageviews: 4, bytes: 1000, errors: 1, duration_ms: 200.0, ..Default::default() };
            h.statuses.insert("2xx".into(), 9);
            h.statuses.insert("5xx".into(), 1);
            h.latency.insert(t::latency_bucket(20.0), 10);
            for v in visitors {
                t::hll_insert(&mut h.visitors, v.wrapping_mul(0x9e3779b97f4a7c15));
            }
            h.paths.insert("/".into(), 6);
            h.paths.insert("/api".into(), 4);
            h.pages.insert("/?token=[redacted]".into(), 4);
            h.referrers.insert("news.example".into(), 3);
            h.countries.insert("ID".into(), 7);
            h.countries.insert("".into(), 3);
            h.params.insert("token".into(), 4);
            h.servers.insert("sg-1".into(), 10);
            h.server_errors.insert("sg-1".into(), 1);
            h.server_latency.insert(format!("sg-1|{}", t::latency_bucket(20.0)), 10);
            h.blocked.insert("office".into(), 2);
            h.blocked_countries.insert("DE".into(), 2);
            h.blocked_paths.insert("/admin".into(), 2);
            h.waf_blocked.insert("942100".into(), 1);
            h.waf_logged.insert("1000001".into(), 2);
            h.waf_countries.insert("b:DE".into(), 1);
            h.waf_countries.insert("l:ID".into(), 2);
            h.waf_paths.insert("b:/login".into(), 1);
            host_row("cl_1", &o, &h).to_string()
        };
        // Three minutes, the same 100 visitors in each, and 50 new ones.
        let rows = [minute(3, "a.example", 0..100), minute(2, "a.example", 0..100), minute(1, "a.example", 50..150)].join("\n");
        ch.insert("traffic_1m", rows.clone(), "cl_1:b1:traffic").await.unwrap();
        // Sent again (the agent didn't hear back): stored once.
        ch.insert("traffic_1m", rows, "cl_1:b1:traffic").await.unwrap();
        // Someone else's host on the same team isn't in these hosts.
        ch.insert("traffic_1m", minute(1, "b.example", 0..5), "cl_1:b2:traffic").await.unwrap();
        let mut p = PortTotals { minute: now - 60, key: "k".into(), protocol: "TCP".into(), connections: 4, bytes_in: 10, bytes_out: 20, ..Default::default() };
        p.outcomes.insert("ok".into(), 3);
        p.outcomes.insert("blocked".into(), 1);
        p.blocked.insert("office".into(), 1);
        p.duration.insert(t::latency_bucket(1000.0), 4);
        t::hll_insert(&mut p.visitors, 42);
        ch.insert("ports_1m", port_row("cl_1", &o, 5432, &p).to_string(), "cl_1:b3:ports").await.unwrap();
        ch.insert("waf_rules", json!({ "team_id": "team_1", "rule": "942100", "message": "SQL Injection", "seen": now }).to_string(), "w").await.unwrap();

        let hosts = vec!["a.example".to_owned()];
        for range in ["1h", "7d"] {
            let a = obs.query("team_1", &hosts, &[], range).await.unwrap();
            let s = &a["summary"];
            assert_eq!(s["requests"], 30.0, "{range}: stored once");
            assert_eq!(s["pageviews"], 12.0);
            // HyperLogLog: within a few percent.
            assert!((s["estimatedVisitors"].as_f64().unwrap() - 150.0).abs() <= 8.0, "{range}: {}", s["estimatedVisitors"]);
            assert_eq!(s["statusCodes"][3], json!({ "code": "5xx", "count": 3.0 }));
            assert_eq!(s["topPaths"][0], json!({ "path": "/", "requests": 18.0 }));
            assert_eq!(s["pages"][0]["path"], "/");
            assert_eq!(s["countries"][1], json!({ "code": "??", "requests": 9.0 }));
            assert_eq!(s["queryParams"][0]["redacted"], true);
            assert!((s["errorRate"].as_f64().unwrap() - 0.1).abs() < 1e-9);
            assert!(s["latency"]["p50"].as_f64().unwrap() > 15.0);
            let points: f64 = a["series"].as_array().unwrap().iter().map(|p| p["requests"].as_f64().unwrap()).sum();
            assert_eq!(points, 30.0, "{range}");
        }
        // The domains list: per host, its own team's only.
        let both = vec!["a.example".to_owned(), "b.example".to_owned()];
        let d = obs.domain_traffic("team_1", &both, "24h").await.unwrap()["domains"].clone();
        assert_eq!(d.as_array().unwrap().len(), 2, "{d}");
        assert_eq!((d[0]["host"].as_str(), d[0]["requests"].as_f64()), (Some("a.example"), Some(30.0)));
        assert_eq!(d[0]["series"].as_array().unwrap().len(), 48);
        assert_eq!(d[1]["requests"], 10.0);
        assert_eq!(obs.domain_traffic("team_2", &both, "24h").await.unwrap()["domains"], json!([]), "another team's hosts");
        let d = obs.domain_traffic("team_1", &both, "7d").await.unwrap()["domains"].clone();
        assert_eq!(d[0]["requests"], 30.0, "from the hours");
        let sum = obs.request_summary("team_1", &hosts, &[], now - 3600).await.unwrap().unwrap();
        assert_eq!((sum["requests"].as_f64(), sum["serverErrors"].as_f64()), (Some(30.0), Some(3.0)));
        let servers = obs.per_server("team_1", &hosts, &[], 24).await.unwrap();
        assert_eq!(servers[0]["node"], "sg-1");
        assert_eq!(servers[0]["errors"], 3.0);
        assert!(servers[0]["p95Ms"].as_f64().unwrap() > 15.0);
        let hourly = obs.per_server_hourly("team_1", &hosts, &[], 24).await.unwrap();
        assert_eq!(hourly.iter().map(|r| r["requests"].as_f64().unwrap()).sum::<f64>(), 30.0);
        let services = vec!["svc_1".to_owned()];
        let b = obs.blocked("team_1", &services, "24h").await.unwrap();
        // Two refusals in each of four minutes (both hosts), and a connection.
        assert_eq!(b["total"], 9.0, "{b}");
        let w = obs.waf_hits("team_1", &services, "24h").await.unwrap();
        assert_eq!((w["blocked"].as_f64(), w["logged"].as_f64()), (Some(4.0), Some(8.0)));
        assert_eq!(w["rules"][0]["message"], "", "the custom rule's message isn't known");
        assert!(w["rules"].as_array().unwrap().iter().any(|r| r["message"] == "SQL Injection"));
        assert_eq!(w["custom"][0]["rule"], 1000001);
        assert_eq!(w["countries"].as_array().unwrap().len(), 2);
        let c = obs.connections("team_1", "svc_1", "1h").await.unwrap();
        assert_eq!(c["ports"][0]["connections"], 4.0);
        assert_eq!(c["ports"][0]["blocked"], 1.0);
        assert_eq!(c["ports"][0]["clients"], 1.0);
    }


    /// Against a throwaway Postgres (DATABASE_URL) and ClickHouse: a
    /// cluster's totals are stored for its own services' hosts and ports
    /// only, and its context names them with their firewalls and the
    /// team's limits.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL and PYO_TEST_CLICKHOUSE_URL (throwaway ones)"]
    async fn a_cluster_stores_only_its_services_traffic() {
        let (Ok(db_url), Ok(ch_url)) = (std::env::var("DATABASE_URL"), std::env::var("PYO_TEST_CLICKHOUSE_URL")) else { return };
        let pool = sqlx::PgPool::connect(&db_url).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let password = std::env::var("PYO_TEST_CLICKHOUSE_PASSWORD").unwrap_or_default();
        let ch = ClickHouse::new(ch_url.clone(), "default".into(), password.clone());
        ch.exec("CREATE DATABASE IF NOT EXISTS pyo_observability", None, &[]).await.unwrap();
        for sql in schema() {
            ch.exec(&sql, None, &[]).await.unwrap();
        }
        let run = crate::ids::new_id("t");
        let (team, one, two) = (format!("team_{run}"), format!("cl1_{run}"), format!("cl2_{run}"));
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug, preferences) VALUES ($1, $1, $1, '{\"requestLogs\": {\"maxMb\": 2048, \"maxDays\": 3}}')").bind(&team).execute(&pool).await.unwrap();
        for c in [&one, &two] {
            q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(c).bind(&team).execute(&pool).await.unwrap();
        }
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'p', 'shop')").bind(format!("prj_{run}")).bind(&team).execute(&pool).await.unwrap();
        let spec = |host: &str| json!({ "domains": [{ "id": "d", "hostname": host }], "ports": [{ "name": "db", "containerPort": 5432, "protocol": "TCP", "exposure": "loadbalancer", "publicPort": 5432 }] });
        let (web, other) = (format!("svc1_{run}"), format!("svc2_{run}"));
        let host = format!("{run}.example.com").to_lowercase();
        let elsewhere = format!("x{run}.example.com").to_lowercase();
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'web', $5)").bind(&web).bind(&team).bind(format!("prj_{run}")).bind(&one).bind(spec(&host)).execute(&pool).await.unwrap();
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'api', $5)").bind(&other).bind(&team).bind(format!("prj_{run}")).bind(&two).bind(spec(&elsewhere)).execute(&pool).await.unwrap();
        q("INSERT INTO pyo.firewall_rules (id, service_id, team_id, action, operator, sources, position) VALUES ($1, $2, $3, 'block', 'in', '{DE}', 0)").bind(format!("fw_{run}")).bind(&web).bind(&team).execute(&pool).await.unwrap();

        let ctx = context(&pool, &one, &team).await.unwrap();
        assert_eq!(ctx.hosts.get(&host), Some(&web));
        assert!(!ctx.hosts.contains_key(&elsewhere), "another cluster's");
        assert_eq!(ctx.ports.get("shop-web-tcp-5432"), Some(&(web.clone(), 5432)));
        assert_eq!(ctx.ports.get("TCP/5432"), Some(&(web.clone(), 5432)));
        assert_eq!(ctx.firewalls[&web].rules[0].sources, ["DE"]);
        assert_eq!((ctx.max_mb, ctx.max_days, ctx.ip_mode.as_str()), (2048, 3, "none"));

        let obs = Observability::with(pool.clone(), Some(ClickHouse::new(ch_url, "default".into(), password)));
        obs.ensure_ready_for_tests().await;
        let minute = pyo_proto::now_ms() / 1000 / 60 * 60 - 60;
        let h = |host: &str| HostTotals { minute, host: host.into(), requests: 5, ..Default::default() };
        let batch = TotalsBatch {
            id: "b1".into(),
            hosts: vec![h(&host.to_uppercase()), h(&elsewhere), h("scanner.example.net"), HostTotals { minute: minute - 30 * 86400, ..h(&host) }],
            ports: vec![PortTotals { minute, key: "shop-web-tcp-5432".into(), protocol: "TCP".into(), connections: 2, ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(obs.ingest_totals(&one, &batch).await.unwrap(), 2, "its own host (any case) and port, this month");
        let stored = ch
            .rows("SELECT host, team_id, service_id, sum(requests) AS n FROM pyo_observability.traffic_1m WHERE team_id = {team:String} GROUP BY host, team_id, service_id".into(), vec![("team", team.clone())])
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!((stored[0]["host"].as_str(), stored[0]["service_id"].as_str(), stored[0]["n"].as_f64()), (Some(host.as_str()), Some(web.as_str()), Some(5.0)));

        // A team that turned observability off: nothing of it is stored.
        q("UPDATE pyo.teams SET observability_on = false WHERE id = $1").bind(&team).execute(&pool).await.unwrap();
        let off = TotalsBatch { id: "b2".into(), hosts: vec![h(&host)], ports: batch.ports.clone(), ..Default::default() };
        assert_eq!(obs.ingest_totals(&one, &off).await.unwrap(), 0);
        // Query strings not kept: no parameter names, pages without their
        // query, also from an agent that sends them.
        q("UPDATE pyo.teams SET observability_on = NULL, query_strings = 'none' WHERE id = $1").bind(&team).execute(&pool).await.unwrap();
        let mut with_query = h(&host);
        with_query.params.insert("q".into(), 5);
        with_query.pages.insert("/search?q=shoes".into(), 5);
        let b3 = TotalsBatch { id: "b3".into(), hosts: vec![with_query], ..Default::default() };
        assert_eq!(obs.ingest_totals(&one, &b3).await.unwrap(), 1);
        let kept = ch
            .rows("SELECT sum(requests) AS n, sumMap(params) AS params, sumMap(pages) AS pages FROM pyo_observability.traffic_1m WHERE team_id = {team:String}".into(), vec![("team", team.clone())])
            .await
            .unwrap();
        assert_eq!(kept[0]["n"].as_f64(), Some(10.0), "b1 and b3, not b2");
        assert_eq!(kept[0]["params"], json!({}));
        assert_eq!(kept[0]["pages"], json!({ "/search": 5 }));
        q("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&pool).await.unwrap();
    }

}
