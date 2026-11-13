//! Observability: website analytics and server metrics. The gateway's
//! access logs come in, per-team traffic goes out.
//!
//! Each cluster's Vector forwards Envoy's JSON access log lines here (with
//! the cluster's agent token). This module turns them into rows and stores
//! them in ClickHouse:
//! - the hostname decides the team, project and service; other traffic is dropped
//! - the client IP is looked up in DB-IP Lite (country, city) and then
//!   discarded unless the IP setting keeps it (anonymized or whole); visitors
//!   are a hash of IP + user agent + host with a salt that
//!   changes daily, so they can't be tied back to anyone
//! - sensitive query parameter values are replaced with "[redacted]"
//!
//! The dashboard's analytics page reads aggregates through `query`.
//!
//! Server metrics live here too: each cluster's heartbeat carries current CPU
//! and memory per server and per service; one sample a minute goes into
//! `pyo_observability.metrics` (kept 30 days) and `metrics` reads it back as
//! time series.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use maxminddb::{Reader, geoip2};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::{Mutex, RwLock};

use crate::error::{ApiError, ApiResult};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS pyo_observability.requests (
    ts DateTime64(3, 'UTC'),
    team_id LowCardinality(String),
    project_id LowCardinality(String),
    service_id LowCardinality(String),
    host LowCardinality(String),
    method LowCardinality(String),
    path String,
    url String,
    query_keys Array(String),
    redacted_keys Array(String),
    status UInt16,
    duration_ms Float32,
    bytes UInt64,
    pageview UInt8,
    visitor UInt64,
    country LowCardinality(String),
    country_name LowCardinality(String),
    city String,
    referrer String,
    device LowCardinality(String),
    browser LowCardinality(String),
    ip String DEFAULT '',
    user_agent String DEFAULT '',
    -- The server that answered (traffic per region).
    node LowCardinality(String) DEFAULT '',
    -- The firewall's refusals: the rule (or blocked, rate-limit when it
    -- can't be told) behind a 403 or 429.
    blocked_by String DEFAULT ''
) ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
ORDER BY (team_id, host, ts)
TTL toDateTime(ts) + INTERVAL 180 DAY";

/// Connections to public TCP and UDP ports through the gateway: one
/// row per connection (per UDP session), written when it ends.
const CONNECTIONS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS pyo_observability.connections (
    ts DateTime64(3, 'UTC'),
    team_id LowCardinality(String),
    project_id LowCardinality(String),
    service_id LowCardinality(String),
    port UInt16,
    protocol LowCardinality(String),
    visitor UInt64,
    ip String DEFAULT '',
    country LowCardinality(String),
    country_name LowCardinality(String),
    city String,
    duration_ms Float64,
    bytes_in UInt64,
    bytes_out UInt64,
    outcome LowCardinality(String),
    node LowCardinality(String) DEFAULT '',
    rule String DEFAULT ''
) ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
ORDER BY (team_id, service_id, ts)
TTL toDateTime(ts) + INTERVAL 180 DAY";

/// The WAF's findings: one row per request where rules matched,
/// blocked or only logged.
const WAF_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS pyo_observability.waf_hits (
    ts DateTime64(3, 'UTC'),
    team_id LowCardinality(String),
    project_id LowCardinality(String),
    service_id LowCardinality(String),
    host LowCardinality(String),
    method LowCardinality(String),
    path String,
    url String,
    ip String DEFAULT '',
    country LowCardinality(String),
    visitor UInt64,
    blocked UInt8,
    rules Array(UInt32),
    messages Array(String),
    severity LowCardinality(String),
    node LowCardinality(String) DEFAULT ''
) ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
ORDER BY (team_id, host, ts)
TTL toDateTime(ts) + INTERVAL 180 DAY";

const METRICS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS pyo_observability.metrics (
    ts DateTime('UTC'),
    team_id LowCardinality(String),
    cluster_id LowCardinality(String),
    kind LowCardinality(String),
    id String,
    name String,
    cpu Float32,
    cpu_capacity Float32,
    memory_mi Float32,
    memory_capacity_mi Float32,
    pods UInt16,
    restarts UInt32,
    -- Disk and network (from the kubelets); null when not read.
    disk_used_gi Nullable(Float32),
    disk_capacity_gi Nullable(Float32),
    net_rx_bps Nullable(Float32),
    net_tx_bps Nullable(Float32),
    storage_mi Nullable(Float32)
) ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
ORDER BY (team_id, kind, id, ts)
TTL ts + INTERVAL 30 DAY";

/// Each server's own figures, from Vector's host_metrics on every node:
/// load, disk I/O and network per real device (counters, as sent) and real
/// filesystems. Rates are worked out when read.
const NODE_METRICS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS pyo_observability.node_metrics (
    ts DateTime('UTC'),
    team_id LowCardinality(String),
    cluster_id LowCardinality(String),
    node LowCardinality(String),
    metric LowCardinality(String),
    device LowCardinality(String) DEFAULT '',
    value Float64
) ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
ORDER BY (team_id, cluster_id, node, metric, ts)
TTL ts + INTERVAL 30 DAY";

/// Database analytics: each database member's figures (CPU, memory, disk,
/// and what its metrics page says: dbmetrics.rs in the agent), one row per
/// figure, about every 30 seconds. Counters arrive as rates per second.
const DATABASE_METRICS_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS pyo_observability.database_metrics (
    ts DateTime('UTC'),
    team_id LowCardinality(String),
    cluster_id LowCardinality(String),
    database_id String,
    member String,
    component LowCardinality(String),
    role LowCardinality(String),
    node LowCardinality(String),
    metric LowCardinality(String),
    value Float64
) ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
ORDER BY (team_id, database_id, metric, member, ts)
TTL ts + INTERVAL 30 DAY";

/// One database metrics sample per cluster this often (the agent reads
/// every member's page every 30s; heartbeats come every 10s).
const DATABASE_SAMPLE_EVERY: Duration = Duration::from_secs(30);

/// The host metrics kept, by Vector's name (with the "host_" namespace dropped).
const HOST_METRICS: [&str; 9] = [
    "load1",
    "load5",
    "load15",
    "filesystem_used_bytes",
    "filesystem_total_bytes",
    "disk_read_bytes_total",
    "disk_written_bytes_total",
    "network_receive_bytes_total",
    "network_transmit_bytes_total",
];

/// Devices that aren't a server's own disks or network cards: container and
/// overlay plumbing, loopbacks, RAM disks.
fn virtual_device(metric: &str, device: &str) -> bool {
    let starts = |ps: &[&str]| ps.iter().any(|p| device.starts_with(p));
    if metric.starts_with("network_") {
        device == "lo"
            || starts(&[
                "veth",
                "cni",
                "flannel",
                "cali",
                "docker",
                "br-",
                "kube-",
                "tunl",
                "vxlan",
                "virbr",
                "lxc",
                "tailscale",
                "wg",
            ])
    } else if metric.starts_with("disk_") {
        starts(&["loop", "ram", "sr", "zram", "nbd"])
    } else {
        false
    }
}

/// One Vector metric event as a node_metrics row, or None when it's not kept.
pub fn host_metric_row(team_id: &str, cluster_id: &str, e: &Value) -> Option<Value> {
    let name = e["name"].as_str()?;
    let name = name.strip_prefix("host_").unwrap_or(name);
    if !HOST_METRICS.contains(&name) {
        return None;
    }
    let tags = &e["tags"];
    let node = tags["node"]
        .as_str()
        .or_else(|| tags["host"].as_str())
        .unwrap_or("");
    if node.is_empty() {
        return None;
    }
    let device = if name.starts_with("filesystem_") {
        // Only real filesystems: the root, and disks of a real type.
        let fs = tags["filesystem"].as_str().unwrap_or("");
        let mount = tags["mountpoint"].as_str().unwrap_or("");
        if !(mount == "/" || matches!(fs, "ext4" | "ext3" | "xfs" | "btrfs" | "zfs"))
            || mount.starts_with("/var/lib/kubelet")
            || mount.starts_with("/run")
        {
            return None;
        }
        mount
    } else {
        tags["device"].as_str().unwrap_or("")
    };
    if virtual_device(name, device) {
        return None;
    }
    let value = e["gauge"]["value"]
        .as_f64()
        .or_else(|| e["counter"]["value"].as_f64())?;
    let ts = e["timestamp"]
        .as_str()
        .and_then(|t| t.parse::<jiff::Timestamp>().ok())
        .map(|t| t.as_second())
        .unwrap_or(pyo_proto::now_ms() / 1000);
    Some(
        json!({ "ts": ts, "team_id": team_id, "cluster_id": cluster_id, "node": node, "metric": name, "device": device, "value": value }),
    )
}

/// A range's buckets for bar charts: how many, and how long each is
/// (seconds).
pub fn bar_steps(range: &str) -> (i64, i64) {
    match range {
        "1h" => (60, 60),
        "24h" => (48, 1800),
        "30d" => (30, 86400),
        _ => (42, 14400),
    }
}

/// Rows with a bucket (`b`, seconds) as one point per bucket of the
/// range's bar chart, ending with the current one (empty buckets are 0).
fn bar_series(rows: &[Value], range: &str, point: impl Fn(Option<&Value>) -> Value) -> Vec<Value> {
    let (points, step) = bar_steps(range);
    let end = (pyo_proto::now_ms() / 1000 / step + 1) * step;
    let start = end - points * step;
    let by_bucket: HashMap<i64, &Value> = rows
        .iter()
        .map(|r| (r["b"].as_i64().unwrap_or(0), r))
        .collect();
    (0..points)
        .map(|i| {
            let b = start + i * step;
            let mut v = point(by_bucket.get(&b).copied());
            v["at"] = json!(b * 1000);
            v
        })
        .collect()
}

/// A range's buckets: how many, and how long each is (seconds).
pub fn range_steps(range: &str) -> (i64, i64) {
    match range {
        "1h" => (60, 60),
        "24h" => (96, 900),
        "30d" => (60, 43200),
        _ => (84, 7200),
    }
}

/// A database's stored figures over a range, as `database_metrics` read them.
pub struct DatabaseRows {
    /// The first bucket (seconds).
    pub start: i64,
    pub step: i64,
    pub points: i64,
    /// `b`, `member`, `metric`, `component`, `role`, `node`, `v`, `mx`.
    pub rows: Vec<Value>,
}

/// A heartbeat's database figures as database_metrics rows (JSON lines):
/// CPU, memory and disk, and every known figure from the page. Members of
/// databases Pyo doesn't have are dropped. `dbs`: (id, team, namespace, name).
pub fn database_metric_rows(
    ts: i64,
    cluster_id: &str,
    dbs: &[(String, String, String, String)],
    members: &[pyo_proto::DatabaseMemberMetrics],
) -> String {
    let mut out = String::new();
    for m in members.iter().take(2000) {
        let Some((id, team, ..)) = dbs.iter().find(|d| d.2 == m.namespace && d.3 == m.database)
        else {
            continue;
        };
        let mut figures: Vec<(&str, f64)> = vec![];
        for (k, v) in [
            ("cpu", m.cpu),
            ("memory_mi", m.memory_mi),
            ("disk_used_gi", m.disk_used_gi),
            ("disk_capacity_gi", m.disk_capacity_gi),
        ] {
            if let Some(v) = v {
                figures.push((k, v));
            }
        }
        for (k, v) in &m.values {
            if let Some(k) = pyo_proto::DATABASE_METRIC_KEYS.iter().find(|x| **x == k) {
                figures.push((k, *v));
            }
        }
        for (metric, value) in figures {
            if !value.is_finite() {
                continue;
            }
            let row = json!({
                "ts": ts, "team_id": team, "cluster_id": cluster_id, "database_id": id, "member": m.member,
                "component": m.component, "role": m.role, "node": m.server, "metric": metric, "value": value,
            });
            out.push_str(&row.to_string());
            out.push('\n');
        }
    }
    out
}

/// What the gateway uses in a cluster (Envoy Gateway's controller and
/// proxies in pyo-gateway), from
/// the heartbeat's workloads. It has no limit of its own, so it's measured
/// against all the cluster's servers.
struct IngressSample {
    cpu: f64,
    cpu_capacity: f64,
    memory_mi: f64,
    memory_capacity_mi: f64,
    pods: u32,
}

fn ingress_sample(hb: &pyo_proto::Heartbeat) -> Option<IngressSample> {
    let parts: Vec<&pyo_proto::WorkloadReport> = hb
        .workloads
        .iter()
        .filter(|w| w.namespace == pyo_proto::system::GATEWAY_NS && w.name.starts_with("envoy-"))
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(IngressSample {
        cpu: parts.iter().map(|w| w.cpu_millis).sum::<f64>() / 1000.0,
        cpu_capacity: hb.servers.iter().map(|n| n.capacity.cpu).sum(),
        memory_mi: parts.iter().map(|w| w.memory_mi).sum(),
        memory_capacity_mi: hb
            .servers
            .iter()
            .map(|n| n.capacity.memory_gi * 1024.0)
            .sum(),
        // The proxies: one per server but build-only ones.
        pods: parts
            .iter()
            .filter(|w| w.kind == "DaemonSet")
            .map(|w| w.ready.max(0) as u32)
            .sum(),
    })
}

/// One metrics sample per cluster this often; heartbeats come every 10s.
const SAMPLE_EVERY: Duration = Duration::from_secs(60);

/// Query parameters whose values are never stored.
const SENSITIVE: [&str; 15] = [
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

pub struct ClickHouse {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
}

impl ClickHouse {
    async fn exec(
        &self,
        sql: &str,
        body: Option<String>,
        params: &[(&str, String)],
    ) -> anyhow::Result<String> {
        let mut q: Vec<(String, String)> = vec![
            ("output_format_json_quote_64bit_integers".into(), "0".into()),
            ("date_time_output_format".into(), "unix_timestamp".into()),
        ];
        for (k, v) in params {
            q.push((format!("param_{k}"), v.clone()));
        }
        let req = self
            .http
            .post(&self.url)
            .query(&q)
            .basic_auth(&self.user, Some(&self.password))
            .timeout(Duration::from_secs(30));
        // Inserts carry the statement in the URL and the rows in the body;
        // everything else sends the statement as the body.
        let res = match body {
            Some(rows) => req.query(&[("query", sql)]).body(rows),
            None => req.body(sql.to_owned()),
        }
        .send()
        .await?;
        let status = res.status();
        let text = res.text().await?;
        if !status.is_success() {
            anyhow::bail!(
                "clickhouse {status}: {}",
                text.chars().take(500).collect::<String>()
            );
        }
        Ok(text)
    }

    async fn rows(&self, sql: String, params: Vec<(&str, String)>) -> anyhow::Result<Vec<Value>> {
        let text = self
            .exec(&format!("{sql} FORMAT JSONEachRow"), None, &params)
            .await?;
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}

#[derive(Clone)]
struct Owner {
    team_id: String,
    project_id: String,
    service_id: String,
    cluster_id: String,
}

/// (cluster, a proxied port's route or "<protocol>/<listen port>") -> its
/// service and public port.
type PortOwners = HashMap<(String, String), (Owner, u16)>;

/// (cluster, service) -> its firewall.
type Firewalls = HashMap<(String, String), pyo_proto::firewall::Firewall>;

/// Filters for `Observability::access_logs`.
pub struct LogFilter {
    pub range: String,
    pub before: i64,
    pub limit: u32,
    pub status: Option<String>,
    pub method: Option<String>,
    pub q: Option<String>,
}

pub struct Observability {
    ch: Option<ClickHouse>,
    /// Swapped in place when geoip.rs downloads a newer database. Mapped,
    /// not read: a lookup touches a few pages of its ~50 MB.
    geo: std::sync::RwLock<Option<Arc<Reader<maxminddb::Mmap>>>>,
    db: PgPool,
    hosts: RwLock<(Instant, HashMap<String, Owner>)>,
    /// (cluster, proxied port's route, or "<protocol>/<listen port>") ->
    /// owning service and the public port, cached 30s.
    routes: RwLock<(Instant, PortOwners)>,
    /// Every service's firewall, to name the rule behind a refusal (30s).
    firewalls: RwLock<(Instant, Firewalls)>,
    salt: Mutex<(String, String)>,
    ready: RwLock<bool>,
    /// When each cluster's last metrics sample was stored.
    sampled: Mutex<HashMap<String, Instant>>,
    /// And its last database metrics sample.
    db_sampled: Mutex<HashMap<String, Instant>>,
    /// The visitor IP setting, re-read every 30s.
    ip_mode: RwLock<(Instant, IpMode)>,
    /// Whether observability is on (the installation's setting), re-read
    /// every 5s: another replica may have changed it.
    on: RwLock<(Instant, bool)>,
}

/// What analytics keeps of a visitor's IP address (an installation-wide
/// setting). Countries, cities and unique visitors work in every mode: they're
/// worked out when the request comes in.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum IpMode {
    /// Nothing (the default).
    #[default]
    None,
    /// With the last part removed: 203.0.113.0, 2001:db8:85a3::.
    Anonymized,
    Full,
}

impl IpMode {
    pub fn apply(self, ip: &str) -> String {
        match (self, ip.parse::<IpAddr>()) {
            (IpMode::None, _) | (_, Err(_)) => String::new(),
            (IpMode::Full, Ok(a)) => a.to_string(),
            (IpMode::Anonymized, Ok(IpAddr::V4(v))) => {
                let [a, b, c, _] = v.octets();
                std::net::Ipv4Addr::new(a, b, c, 0).to_string()
            }
            (IpMode::Anonymized, Ok(IpAddr::V6(v))) => {
                let s = v.segments();
                std::net::Ipv6Addr::new(s[0], s[1], s[2], 0, 0, 0, 0, 0).to_string()
            }
        }
    }
}

impl AccessLine {
    /// The visitor: the address the proxy placed them at (a PROXY protocol
    /// header's, else the connection's), as its firewall and rate limits
    /// see them; the TCP peer for lines without it. Never X-Forwarded-For
    /// or X-Real-Ip: whoever reaches the proxy could write any address there
    /// (pyo-proto's `client_policy_spec`).
    pub fn visitor_ip(&self) -> &str {
        if self.remote.is_empty() {
            &self.peer
        } else {
            &self.remote
        }
    }
}

/// One access log line from the gateway (Envoy's JSON, fields in
/// pyo-proto's system.rs).
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

impl AccessLine {
    pub fn read(l: &Value) -> Self {
        // Envoy writes null for a header that wasn't there.
        let s = |k: &str| l[k].as_str().unwrap_or("").to_owned();
        let n = |k: &str| {
            l[k].as_u64()
                .or_else(|| l[k].as_str().and_then(|v| v.parse().ok()))
                .unwrap_or(0)
        };
        let ts = |k: &str| {
            l[k].as_str()
                .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                .map(|t| t.as_millisecond())
        };
        AccessLine {
            host: s("host"),
            ts_ms: ts("start_time"),
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
            let proto = if self.upstream.starts_with("tcp") {
                "TCP"
            } else {
                "UDP"
            };
            // <namespace>/<route>/rule/<n>
            let route = rest.split('/').nth(1)?;
            return Some((proto, PortKey::Route(route)));
        }
        let port = u16::try_from(self.local_port).ok().filter(|p| *p > 0)?;
        if !self.upstream.is_empty() {
            return None;
        }
        Some((
            if self.upstream_host.is_empty() {
                "TCP"
            } else {
                "UDP"
            },
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

/// What the API answers for history while observability is off.
pub fn off() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "Observability is off.",
    )
}

/// Whether observability is on (the installation's setting, 'enabled' in
/// pyo.settings 'observability'). On unless turned off: a fresh install
/// saves its default with its first admin (`first_default`).
pub async fn setting(db: &PgPool) -> ApiResult<bool> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'observability'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| v["enabled"].as_bool()).unwrap_or(true))
}

/// A fresh install's observability: off on a small server (under
/// 3.5 GB of memory, as the installers count it), else on. Saved once, with
/// the first admin. Later it's the admin's: a server given more memory gets
/// a hint in the dashboard, never a silent change.
pub async fn first_default(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> sqlx::Result<bool> {
    let on = pyo_proto::memory::server_tier() != pyo_proto::memory::Tier::Small;
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('observability', jsonb_build_object('enabled', $1)) ON CONFLICT (key) DO NOTHING",
    )
    .bind(on)
    .execute(&mut **tx)
    .await?;
    Ok(on)
}

/// The saved setting ('observability' in pyo.settings).
pub async fn ip_mode(db: &PgPool) -> ApiResult<IpMode> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'observability'")
            .fetch_optional(db)
            .await?;
    Ok(
        v.and_then(|v| serde_json::from_value(v["ipMode"].clone()).ok())
            .unwrap_or_default(),
    )
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// One server's or service's metrics: cluster, name, a row per bucket.
type Series<'a> = (String, String, Vec<Option<&'a Value>>);

impl Observability {
    pub fn from_env(db: PgPool) -> Arc<Self> {
        let ch = env("CLICKHOUSE_URL").map(|url| ClickHouse {
            http: reqwest::Client::new(),
            url,
            user: env("CLICKHOUSE_USER").unwrap_or_else(|| "default".into()),
            password: env("CLICKHOUSE_PASSWORD").unwrap_or_default(),
        });
        // geoip.rs loads the database once it has one.
        let geo: Option<Reader<maxminddb::Mmap>> = None;
        if ch.is_none() {
            tracing::warn!("CLICKHOUSE_URL isn't set; observability is off");
        }
        Arc::new(Self {
            ch,
            geo: std::sync::RwLock::new(geo.map(Arc::new)),
            db,
            hosts: RwLock::new((Instant::now() - Duration::from_secs(3600), HashMap::new())),
            routes: RwLock::new((Instant::now() - Duration::from_secs(3600), HashMap::new())),
            firewalls: RwLock::new((Instant::now() - Duration::from_secs(3600), HashMap::new())),
            salt: Mutex::new((String::new(), String::new())),
            ready: RwLock::new(false),
            sampled: Mutex::new(HashMap::new()),
            db_sampled: Mutex::new(HashMap::new()),
            ip_mode: RwLock::new((Instant::now() - Duration::from_secs(60), IpMode::None)),
            on: RwLock::new((Instant::now() - Duration::from_secs(60), true)),
        })
    }

    /// Whether observability is on: ClickHouse is configured and the
    /// installation's setting is on.
    pub async fn enabled(&self) -> bool {
        if self.ch.is_none() {
            return false;
        }
        {
            let g = self.on.read().await;
            if g.0.elapsed() < Duration::from_secs(5) {
                return g.1;
            }
        }
        let Ok(on) = setting(&self.db).await else {
            return self.on.read().await.1;
        };
        *self.on.write().await = (Instant::now(), on);
        on
    }

    /// After this replica changed the setting: no waiting for the cache.
    pub async fn set_enabled(&self, on: bool) {
        *self.on.write().await = (Instant::now(), on);
    }

    /// ClickHouse, while observability is on.
    async fn store(&self) -> Option<&ClickHouse> {
        if self.enabled().await {
            self.ch.as_ref()
        } else {
            None
        }
    }

    /// ClickHouse for a reader, or why there's nothing to read.
    async fn reader(&self) -> ApiResult<&ClickHouse> {
        let Some(ch) = &self.ch else {
            return Err(ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Observability storage isn't configured on this installation.",
            ));
        };
        if !self.enabled().await {
            return Err(off());
        }
        Ok(ch)
    }

    /// Create the tables, retrying while ClickHouse starts. While
    /// observability is off, waits until it's turned on.
    pub async fn ensure_schema(self: Arc<Self>) {
        let Some(ch) = &self.ch else { return };
        loop {
            if !self.enabled().await {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            let r = async {
                ch.exec("CREATE DATABASE IF NOT EXISTS pyo_observability", None, &[])
                    .await?;
                ch.exec(SCHEMA, None, &[]).await?;
                ch.exec(CONNECTIONS_SCHEMA, None, &[]).await?;
                ch.exec(WAF_SCHEMA, None, &[]).await?;
                ch.exec(METRICS_SCHEMA, None, &[]).await?;
                ch.exec(NODE_METRICS_SCHEMA, None, &[]).await?;
                ch.exec(DATABASE_METRICS_SCHEMA, None, &[]).await
            }
            .await;
            match r {
                Ok(_) => {
                    *self.ready.write().await = true;
                    tracing::info!("observability storage ready");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ClickHouse not ready; retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// hostname -> owning service, from the domains of every service (cached 30s).
    async fn owners(&self) -> anyhow::Result<HashMap<String, Owner>> {
        {
            let g = self.hosts.read().await;
            if g.0.elapsed() < Duration::from_secs(30) {
                return Ok(g.1.clone());
            }
        }
        let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT lower(d->>'hostname'), a.team_id, a.project_id, a.id, a.cluster_id
               FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
              WHERE NOT a.deleting",
        )
        .fetch_all(&self.db)
        .await?;
        let map: HashMap<String, Owner> = rows
            .into_iter()
            .map(|(h, team_id, project_id, service_id, cluster_id)| {
                (
                    h,
                    Owner {
                        team_id,
                        project_id,
                        service_id,
                        cluster_id,
                    },
                )
            })
            .collect();
        *self.hosts.write().await = (Instant::now(), map.clone());
        Ok(map)
    }

    /// Each proxied TCP and UDP port's route (as the agent names it,
    /// `<namespace>-<service>-tcp-<port>`) -> its service and port.
    async fn port_routes(&self) -> anyhow::Result<PortOwners> {
        {
            let g = self.routes.read().await;
            if g.0.elapsed() < Duration::from_secs(30) {
                return Ok(g.1.clone());
            }
        }
        let rows: Vec<(String, String, String, String, String, String, Value)> = sqlx::query_as(
            "SELECT a.team_id, a.project_id, a.id, a.cluster_id, p.namespace, a.name, a.spec->'ports'
               FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id
              WHERE NOT a.deleting",
        )
        .fetch_all(&self.db)
        .await?;
        let mut map = HashMap::new();
        for (team_id, project_id, service_id, cluster_id, ns, name, ports) in rows {
            for p in ports
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(crate::deploy::port_spec)
                .filter(|p| p.proxied())
            {
                let route = format!(
                    "{ns}-{name}-{}-{}",
                    p.transport().to_lowercase(),
                    p.public()
                );
                let owner = Owner {
                    team_id: team_id.clone(),
                    project_id: project_id.clone(),
                    service_id: service_id.clone(),
                    cluster_id: cluster_id.clone(),
                };
                let listen = format!(
                    "{}/{}",
                    p.transport(),
                    pyo_proto::proxy_listen_port(p.public())
                );
                map.insert(
                    (cluster_id.clone(), listen),
                    (owner.clone(), p.public() as u16),
                );
                map.insert((cluster_id.clone(), route), (owner, p.public() as u16));
            }
        }
        *self.routes.write().await = (Instant::now(), map.clone());
        Ok(map)
    }

    /// The rule behind a refusal: the rule that matches the address (or its
    /// country), else what the proxy said.
    async fn refusal_rule(
        &self,
        cluster_id: &str,
        service_id: &str,
        why: String,
        ip: &str,
        country: &str,
    ) -> String {
        if why != "blocked" && why != "rate-limit" {
            return why;
        }
        let stale = self.firewalls.read().await.0.elapsed() >= Duration::from_secs(30);
        if stale && let Ok(all) = crate::firewall::all_firewalls(&self.db).await {
            *self.firewalls.write().await = (Instant::now(), all);
        }
        let g = self.firewalls.read().await;
        let (Some(f), Ok(addr)) = (
            g.1.get(&(cluster_id.to_owned(), service_id.to_owned())),
            ip.parse::<IpAddr>(),
        ) else {
            return why;
        };
        let rule = if why == "blocked" {
            pyo_proto::firewall::blocking_rule(&[f], addr, Some(country).filter(|c| !c.is_empty()))
        } else {
            pyo_proto::firewall::rate_limiting_rule(&[f], addr)
        };
        rule.map(str::to_owned).unwrap_or(why)
    }

    async fn current_ip_mode(&self) -> IpMode {
        {
            let g = self.ip_mode.read().await;
            if g.0.elapsed() < Duration::from_secs(30) {
                return g.1;
            }
        }
        let mode = ip_mode(&self.db).await.unwrap_or_default();
        *self.ip_mode.write().await = (Instant::now(), mode);
        mode
    }

    /// Today's salt for visitor hashes, shared through Postgres; older ones are deleted.
    async fn daily_salt(&self, day: &str) -> anyhow::Result<String> {
        let mut g = self.salt.lock().await;
        if g.0 == day {
            return Ok(g.1.clone());
        }
        let fresh = crate::ids::secret_token();
        let salt: String = sqlx::query_scalar(
            "INSERT INTO pyo.observability_salts (day, salt) VALUES ($1::date, $2)
             ON CONFLICT (day) DO UPDATE SET day = excluded.day RETURNING salt",
        )
        .bind(day)
        .bind(&fresh)
        .fetch_one(&self.db)
        .await?;
        sqlx::query("DELETE FROM pyo.observability_salts WHERE day < $1::date - 1")
            .bind(day)
            .execute(&self.db)
            .await?;
        *g = (day.to_owned(), salt.clone());
        Ok(salt)
    }

    /// Store a batch of access log lines (the gateway's) from one cluster.
    pub async fn ingest(&self, cluster_id: &str, lines: Vec<Value>) -> ApiResult<usize> {
        let Some(ch) = self.store().await else {
            return Ok(0);
        };
        if !*self.ready.read().await {
            return Err(ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Observability storage is starting.",
            ));
        }
        let owners = self.owners().await?;
        let ua_parser = woothee::parser::Parser::new();
        let mut out = String::new();
        let mut conns = String::new();
        let mut waf = String::new();
        let mut n = 0;
        for l in lines {
            // The WAF's finding for one request (pyo-waf's line).
            if l.get("waf").is_some() {
                let host = l["host"].as_str().unwrap_or("").to_lowercase();
                let Some(owner) = owners.get(&host).filter(|o| o.cluster_id == cluster_id) else {
                    continue;
                };
                let ts = l["time"]
                    .as_str()
                    .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                    .map(|t| t.as_millisecond())
                    .unwrap_or_else(pyo_proto::now_ms);
                let day = jiff::Timestamp::from_millisecond(ts)
                    .map(|t| t.strftime("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let salt = self.daily_salt(&day).await?;
                let ip = l["client"].as_str().unwrap_or("").to_owned();
                let (country, _, _) = self.locate(&ip);
                let (path, url, _, _) = split_url(l["uri"].as_str().unwrap_or("/"));
                let visitor = {
                    let d = Sha256::digest(format!("{salt}|{ip}|waf|{host}").as_bytes());
                    u64::from_be_bytes(d[..8].try_into().unwrap_or([0; 8]))
                };
                let row = json!({
                    "ts": ts as f64 / 1000.0,
                    "team_id": owner.team_id, "project_id": owner.project_id, "service_id": owner.service_id,
                    "host": host, "method": l["method"].as_str().unwrap_or("").chars().take(16).collect::<String>(),
                    "path": path, "url": url,
                    "ip": self.current_ip_mode().await.apply(&ip), "country": country, "visitor": visitor,
                    "blocked": l["blocked"].as_bool().unwrap_or(false) as u8,
                    "rules": l["rules"].as_array().into_iter().flatten().filter_map(Value::as_u64).take(50).collect::<Vec<_>>(),
                    "messages": l["messages"].as_array().into_iter().flatten().filter_map(Value::as_str).take(50).map(|m| m.chars().take(300).collect::<String>()).collect::<Vec<_>>(),
                    "severity": l["severity"].as_str().unwrap_or("").chars().take(16).collect::<String>(),
                    "node": l["PyoNode"].as_str().unwrap_or("").chars().take(253).collect::<String>(),
                });
                waf.push_str(&row.to_string());
                waf.push('\n');
                n += 1;
                continue;
            }
            let l = AccessLine::read(&l);
            // A connection to a public TCP or UDP port.
            if let Some((proto, key)) = l.port_key() {
                let routes = self.port_routes().await?;
                let key = match key {
                    PortKey::Route(r) => r.to_owned(),
                    PortKey::Listen(port) => format!("{proto}/{port}"),
                };
                let Some((owner, port)) = routes.get(&(cluster_id.to_owned(), key)) else {
                    continue;
                };
                let ts = l.ts_ms.unwrap_or_else(pyo_proto::now_ms);
                let day = jiff::Timestamp::from_millisecond(ts)
                    .map(|t| t.strftime("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let salt = self.daily_salt(&day).await?;
                let ip = l.visitor_ip().to_owned();
                let (country, country_name, city) = self.locate(&ip);
                let visitor = {
                    let d = Sha256::digest(
                        format!("{salt}|{ip}|{}|{port}", owner.service_id).as_bytes(),
                    );
                    u64::from_be_bytes(d[..8].try_into().unwrap_or([0; 8]))
                };
                let row = json!({
                    "ts": ts as f64 / 1000.0,
                    "team_id": owner.team_id, "project_id": owner.project_id, "service_id": owner.service_id,
                    "port": port, "protocol": proto, "visitor": visitor,
                    "ip": self.current_ip_mode().await.apply(&ip),
                    "country": country, "country_name": country_name, "city": city,
                    "duration_ms": l.duration_ms, "bytes_in": l.bytes_in, "bytes_out": l.bytes,
                    "outcome": l.outcome(),
                    "node": l.node.chars().take(253).collect::<String>(),
                    "rule": self.refusal_rule(cluster_id, &owner.service_id, crate::firewall::refusal("", &l.termination, 0), &ip, &country).await,
                });
                conns.push_str(&row.to_string());
                conns.push('\n');
                n += 1;
                continue;
            }
            let host = l.host.split(':').next().unwrap_or("").to_lowercase();
            // Only traffic for a domain of a service on this cluster.
            let Some(owner) = owners.get(&host).filter(|o| o.cluster_id == cluster_id) else {
                continue;
            };
            let ts = l.ts_ms.unwrap_or_else(pyo_proto::now_ms);
            let day = jiff::Timestamp::from_millisecond(ts)
                .map(|t| t.strftime("%Y-%m-%d").to_string())
                .unwrap_or_default();
            let salt = self.daily_salt(&day).await?;
            let ip = l.visitor_ip();
            let ua = l.user_agent.as_str();
            let status = l.status;
            let method = if l.method.is_empty() {
                "GET"
            } else {
                l.method.as_str()
            };
            let (path, url, keys, redacted) = split_url(if l.path.is_empty() {
                "/"
            } else {
                l.path.as_str()
            });
            let ctype = l.content_type.as_str();

            let parsed = ua_parser.parse(ua);
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
                .unwrap_or(if ua.starts_with("curl/") {
                    "curl"
                } else {
                    "Other"
                });
            // A page view: a GET answered with HTML (a 304 has no body, so a path without an extension).
            let html = ctype.starts_with("text/html")
                || (status == 304
                    && ctype.is_empty()
                    && !path.rsplit('/').next().unwrap_or("").contains('.'));
            let pageview = method == "GET" && (200..400).contains(&status) && html && !crawler;

            let referrer = Some(l.referer.as_str())
                .filter(|r| !r.is_empty())
                .and_then(|r| r.split("://").nth(1))
                .map(|r| r.split(['/', '?', ':']).next().unwrap_or("").to_lowercase());
            let referrer = match referrer.as_deref() {
                None | Some("") => "Direct".to_owned(),
                Some(r) if r == host => String::new(),
                Some(r) => r.trim_start_matches("www.").to_owned(),
            };

            let (country, country_name, city) = self.locate(ip);
            let stored_ip = self.current_ip_mode().await.apply(ip);
            let visitor = {
                let d = Sha256::digest(format!("{salt}|{ip}|{ua}|{host}").as_bytes());
                u64::from_be_bytes(d[..8].try_into().unwrap_or([0; 8]))
            };

            let row = json!({
                "ts": ts as f64 / 1000.0,
                "team_id": owner.team_id, "project_id": owner.project_id, "service_id": owner.service_id,
                "host": host, "method": method, "path": path, "url": url,
                "query_keys": keys, "redacted_keys": redacted,
                "status": status,
                "duration_ms": l.duration_ms,
                "bytes": l.bytes,
                "pageview": pageview as u8,
                "visitor": visitor,
                "country": country, "country_name": country_name, "city": city,
                "referrer": referrer, "device": device, "browser": browser,
                "ip": stored_ip,
                // As sent, for the request log; capped like a header line.
                "user_agent": ua.chars().take(512).collect::<String>(),
                "node": l.node.chars().take(253).collect::<String>(),
                "blocked_by": self.refusal_rule(cluster_id, &owner.service_id, crate::firewall::refusal(&l.details, "", status), ip, &country).await,
            });
            out.push_str(&row.to_string());
            out.push('\n');
            n += 1;
        }
        if !out.is_empty() {
            ch.exec(
                "INSERT INTO pyo_observability.requests FORMAT JSONEachRow",
                Some(out),
                &[],
            )
            .await?;
        }
        if !conns.is_empty() {
            ch.exec(
                "INSERT INTO pyo_observability.connections FORMAT JSONEachRow",
                Some(conns),
                &[],
            )
            .await?;
        }
        if !waf.is_empty() {
            ch.exec(
                "INSERT INTO pyo_observability.waf_hits FORMAT JSONEachRow",
                Some(waf),
                &[],
            )
            .await?;
        }
        Ok(n)
    }

    /// Store a heartbeat's CPU and memory figures, at most once a minute per
    /// cluster. Best effort: a failure is logged, never returned.
    pub async fn record_metrics(&self, team_id: &str, cluster_id: &str, hb: &pyo_proto::Heartbeat) {
        let Some(ch) = self.store().await else { return };
        if !*self.ready.read().await {
            return;
        }
        {
            let mut g = self.sampled.lock().await;
            if g.get(cluster_id)
                .is_some_and(|t| t.elapsed() < SAMPLE_EVERY - Duration::from_secs(5))
            {
                return;
            }
            g.insert(cluster_id.to_owned(), Instant::now());
        }
        let r = async {
            // Limits come from each service's settings; the agent reports usage only.
            let services: Vec<(String, String, f64, f64)> = sqlx::query_as(
                "SELECT id, name, coalesce((spec->'resources'->>'cpuLimit')::float8, 500), coalesce((spec->'resources'->>'memoryLimit')::float8, 512)
                   FROM pyo.services WHERE cluster_id = $1 AND NOT deleting",
            )
            .bind(cluster_id)
            .fetch_all(&self.db)
            .await?;
            let ts = pyo_proto::now_ms() / 1000;
            let mut out = String::new();
            let mut row = |kind: &str, id: &str, name: &str, cpu: f64, cpu_cap: f64, mem: f64, mem_cap: f64, pods: u32, restarts: i64, extra: Value| {
                let mut r = json!({
                    "ts": ts, "team_id": team_id, "cluster_id": cluster_id, "kind": kind, "id": id, "name": name,
                    "cpu": cpu, "cpu_capacity": cpu_cap, "memory_mi": mem, "memory_capacity_mi": mem_cap, "pods": pods, "restarts": restarts.max(0),
                });
                if let (Some(r), Some(e)) = (r.as_object_mut(), extra.as_object()) {
                    r.extend(e.clone());
                }
                out.push_str(&r.to_string());
                out.push('\n');
            };
            for n in &hb.servers {
                let u = &n.usage;
                let extra = json!({
                    "disk_used_gi": u.root_fs.map(|f| f.used_gi), "disk_capacity_gi": u.root_fs.map(|f| f.capacity_gi),
                    "net_rx_bps": u.net_rx_bps, "net_tx_bps": u.net_tx_bps,
                });
                row("server", &n.name, &n.name, u.cpu, n.capacity.cpu, u.memory_gi * 1024.0, n.capacity.memory_gi * 1024.0, n.pod_count, 0, extra);
            }
            for a in &hb.services {
                let Some((id, name, cpu_limit, mem_limit)) = services.iter().find(|x| x.0 == a.service_id) else { continue };
                let n = a.desired.max(0) as f64;
                let extra = json!({ "storage_mi": a.storage_mi, "net_rx_bps": a.net_rx_bps, "net_tx_bps": a.net_tx_bps });
                row("service", id, name, a.cpu_millis / 1000.0, cpu_limit * n / 1000.0, a.memory_mi, mem_limit * n, a.ready_replicas.max(0) as u32, a.restarts as i64, extra);
            }
            if let Some(t) = ingress_sample(hb) {
                row("ingress", cluster_id, "Gateway", t.cpu, t.cpu_capacity, t.memory_mi, t.memory_capacity_mi, t.pods, 0, json!({}));
            }
            if !out.is_empty() {
                ch.exec("INSERT INTO pyo_observability.metrics FORMAT JSONEachRow", Some(out), &[]).await?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(cluster = %cluster_id, error = %e, "metrics sample not stored");
        }
    }

    /// Store a heartbeat's database member figures, at most every 30s per
    /// cluster, under the database's id (and its team: databases are the
    /// team's). Best effort, like `record_metrics`.
    pub async fn record_database_metrics(&self, cluster_id: &str, hb: &pyo_proto::Heartbeat) {
        let Some(ch) = self.store().await else { return };
        if hb.database_metrics.is_empty() || !*self.ready.read().await {
            return;
        }
        {
            let mut g = self.db_sampled.lock().await;
            if g.get(cluster_id)
                .is_some_and(|t| t.elapsed() < DATABASE_SAMPLE_EVERY - Duration::from_secs(3))
            {
                return;
            }
            g.insert(cluster_id.to_owned(), Instant::now());
        }
        let r = async {
            let dbs: Vec<(String, String, String, String)> = sqlx::query_as(
                "SELECT id, team_id, namespace, name FROM pyo.databases WHERE cluster_id = $1",
            )
            .bind(cluster_id)
            .fetch_all(&self.db)
            .await?;
            let ts = pyo_proto::now_ms() / 1000;
            let out = database_metric_rows(ts, cluster_id, &dbs, &hb.database_metrics);
            if !out.is_empty() {
                ch.exec(
                    "INSERT INTO pyo_observability.database_metrics FORMAT JSONEachRow",
                    Some(out),
                    &[],
                )
                .await?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(cluster = %cluster_id, error = %e, "database metrics sample not stored");
        }
    }

    /// One database's figures over `range`, per bucket, member and figure:
    /// averaged over the bucket (for lag also its worst). The shaping is
    /// database_analytics.rs's.
    pub async fn database_metrics(
        &self,
        team_id: &str,
        database_id: &str,
        range: &str,
    ) -> ApiResult<DatabaseRows> {
        let ch = self.reader().await?;
        let (points, step) = range_steps(range);
        let now = pyo_proto::now_ms() / 1000;
        let end = (now / step + 1) * step;
        let start = end - points * step;
        let rows = ch
            .rows(
                "SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({step:Int64}))) AS b, member, metric,
                        argMax(component, ts) AS component, argMax(role, ts) AS role, argMax(node, ts) AS node,
                        avg(value) AS v, max(value) AS mx
                   FROM pyo_observability.database_metrics
                  WHERE team_id = {team:String} AND database_id = {db:String}
                    AND ts >= toDateTime({start:Int64}) AND ts < toDateTime({end:Int64})
                  GROUP BY b, member, metric ORDER BY b"
                    .into(),
                vec![("team", team_id.to_owned()), ("db", database_id.to_owned()), ("start", start.to_string()), ("end", end.to_string()), ("step", step.to_string())],
            )
            .await?;
        Ok(DatabaseRows {
            start,
            step,
            points,
            rows,
        })
    }

    /// Store host metrics from a cluster's Vector. Returns rows kept.
    pub async fn ingest_host_metrics(
        &self,
        team_id: &str,
        cluster_id: &str,
        events: &[Value],
    ) -> ApiResult<usize> {
        let Some(ch) = self.store().await else {
            return Ok(0);
        };
        if !*self.ready.read().await {
            return Ok(0);
        }
        let mut out = String::new();
        let mut n = 0;
        // A batch is a few hundred events; more is someone sending junk.
        for e in events.iter().take(5000) {
            if let Some(r) = host_metric_row(team_id, cluster_id, e) {
                out.push_str(&r.to_string());
                out.push('\n');
                n += 1;
            }
        }
        if n > 0 {
            ch.exec(
                "INSERT INTO pyo_observability.node_metrics FORMAT JSONEachRow",
                Some(out),
                &[],
            )
            .await?;
        }
        Ok(n)
    }

    /// CPU and memory over time for a team's servers and services (optionally one
    /// cluster's). Series start at the first sample in the range; a gap
    /// repeats the previous value.
    pub async fn metrics(&self, team_id: &str, cluster_id: &str, range: &str) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let (points, step) = range_steps(range);
        let now = pyo_proto::now_ms() / 1000;
        let end = (now / step + 1) * step;
        let start = end - points * step;
        let rows = ch
            .rows(
                "SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({step:Int64}))) AS b, kind, id, any(cluster_id) AS cluster, any(name) AS name,
                        avg(cpu) AS cpu, max(cpu_capacity) AS cpu_cap, avg(memory_mi) AS mem, max(memory_capacity_mi) AS mem_cap,
                        max(pods) AS pods, max(restarts) AS restarts,
                        avg(disk_used_gi) AS disk_used, max(disk_capacity_gi) AS disk_cap,
                        avg(net_rx_bps) AS net_rx, avg(net_tx_bps) AS net_tx, avg(storage_mi) AS storage
                   FROM pyo_observability.metrics
                  WHERE team_id = {team:String} AND ts >= toDateTime({start:Int64}) AND ts < toDateTime({end:Int64})
                    AND ({cluster:String} = '' OR cluster_id = {cluster:String})
                  GROUP BY b, kind, id ORDER BY b"
                    .into(),
                vec![("team", team_id.to_owned()), ("cluster", cluster_id.to_owned()), ("start", start.to_string()), ("end", end.to_string()), ("step", step.to_string())],
            )
            .await?;

        let num = |v: &Value, k: &str| v[k].as_f64().filter(|x| x.is_finite()).unwrap_or(0.0);
        let first = rows.iter().filter_map(|r| r["b"].as_i64()).min();
        let Some(first) = first else {
            return Ok(
                json!({ "range": range, "stepMs": step * 1000, "times": [], "servers": [], "services": [], "ingress": [] }),
            );
        };
        let from = ((first - start) / step).clamp(0, points - 1);
        let times: Vec<i64> = (from..points).map(|i| (start + i * step) * 1000).collect();
        let idx = |b: i64| ((b - start) / step - from) as usize;

        // kind/id -> (cluster, name, per-bucket rows)
        let mut by: std::collections::BTreeMap<(String, String), Series<'_>> =
            std::collections::BTreeMap::new();
        for r in &rows {
            let key = (
                r["kind"].as_str().unwrap_or("").to_owned(),
                r["id"].as_str().unwrap_or("").to_owned(),
            );
            let e = by.entry(key).or_insert_with(|| {
                (
                    r["cluster"].as_str().unwrap_or("").to_owned(),
                    String::new(),
                    vec![None; times.len()],
                )
            });
            e.1 = r["name"].as_str().unwrap_or("").to_owned();
            let i = idx(r["b"].as_i64().unwrap_or(start));
            if i < times.len() {
                e.2[i] = Some(r);
            }
        }
        // A missed sample or two is bridged with the last value; a longer
        // silence (the cluster or Pyo was down) stays a gap (null).
        let series = |cells: &[Option<&Value>], k: &str| -> Vec<Option<f64>> {
            let mut last: Option<f64> = None;
            let mut missed = 0;
            cells
                .iter()
                .map(|c| match c {
                    Some(c) => {
                        missed = 0;
                        last = Some((num(c, k) * 1000.0).round() / 1000.0);
                        last
                    }
                    None => {
                        missed += 1;
                        if missed <= 2 { last } else { None }
                    }
                })
                .collect()
        };
        let latest = |cells: &[Option<&Value>], k: &str| {
            cells
                .iter()
                .rev()
                .flatten()
                .next()
                .map(|c| num(c, k))
                .unwrap_or(0.0)
        };
        // Figures older agents don't send: null stays null (shown as "—"), never 0.
        let maybe = |cells: &[Option<&Value>], k: &str| -> Vec<Option<f64>> {
            cells
                .iter()
                .map(|c| {
                    c.and_then(|c| c[k].as_f64())
                        .filter(|x| x.is_finite())
                        .map(|x| (x * 1000.0).round() / 1000.0)
                })
                .collect()
        };
        let latest_maybe = |cells: &[Option<&Value>], k: &str| {
            cells.iter().rev().flatten().find_map(|c| c[k].as_f64())
        };
        let entity =
            |(kind, id): &(String, String),
             (cluster, name, cells): &(String, String, Vec<Option<&Value>>)| {
                let mut e = json!({
                    "id": id, "name": name, "clusterId": cluster,
                    "cpu": series(cells, "cpu"), "memoryMi": series(cells, "mem"),
                    "cpuCapacity": latest(cells, "cpu_cap"), "memoryCapacityMi": latest(cells, "mem_cap"),
                    "pods": latest(cells, "pods"), "restarts": latest(cells, "restarts"),
                    "netRxBps": maybe(cells, "net_rx"), "netTxBps": maybe(cells, "net_tx"),
                });
                if kind == "server" {
                    e["diskUsedGi"] = json!(maybe(cells, "disk_used"));
                    e["diskCapacityGi"] = json!(latest_maybe(cells, "disk_cap"));
                } else if kind == "service" {
                    e["storageMi"] = json!(maybe(cells, "storage"));
                }
                e
            };
        let mut servers: Vec<Value> = by
            .iter()
            .filter(|(k, _)| k.0 == "server")
            .map(|(k, v)| entity(k, v))
            .collect();
        // The servers' own figures from Vector: load, disk I/O, network cards.
        let host = self
            .host_series(ch, team_id, cluster_id, start, end, step, &times)
            .await
            .unwrap_or_default();
        for n in &mut servers {
            let key = (
                n["clusterId"].as_str().unwrap_or("").to_owned(),
                n["id"].as_str().unwrap_or("").to_owned(),
            );
            let empty = vec![None; times.len()];
            let get = |m: &str| {
                host.get(&(key.0.clone(), key.1.clone(), m.to_owned()))
                    .cloned()
                    .unwrap_or_else(|| empty.clone())
            };
            n["load1"] = json!(get("load1"));
            n["diskReadBps"] = json!(get("disk_read_bytes_total"));
            n["diskWriteBps"] = json!(get("disk_written_bytes_total"));
            n["nicRxBps"] = json!(get("network_receive_bytes_total"));
            n["nicTxBps"] = json!(get("network_transmit_bytes_total"));
        }
        let services: Vec<Value> = by
            .iter()
            .filter(|(k, _)| k.0 == "service")
            .map(|(k, v)| entity(k, v))
            .collect();
        let ingress: Vec<Value> = by
            .iter()
            .filter(|(k, _)| k.0 == "ingress")
            .map(|(k, v)| entity(k, v))
            .collect();
        Ok(
            json!({ "range": range, "stepMs": step * 1000, "times": times, "servers": servers, "services": services, "ingress": ingress }),
        )
    }

    /// Host metrics per (cluster, node, metric), aligned to `times`: load as
    /// an average, counters as bytes per second (deltaSumTimestamp skips a
    /// counter going back, so a reboot doesn't show as a spike), summed over
    /// a server's devices.
    #[allow(clippy::too_many_arguments)]
    async fn host_series(
        &self,
        ch: &ClickHouse,
        team_id: &str,
        cluster_id: &str,
        start: i64,
        end: i64,
        step: i64,
        times: &[i64],
    ) -> ApiResult<HashMap<(String, String, String), Vec<Option<f64>>>> {
        let rows = ch
            .rows(
                // Counters become a rate per sample, from the one before it
                // on the same device: none across a reset (it went down) or a
                // gap of over ten minutes. Then averaged per bucket and device,
                // and summed over a server's devices.
                "SELECT b, cluster_id, node, metric, sum(r) AS v FROM (
                   SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({step:Int64}))) AS b, cluster_id, node, metric, device, avg(r) AS r FROM (
                     SELECT ts, cluster_id, node, metric, device,
                            if(endsWith(metric, '_total'),
                               if(value >= pv AND t - pt > 0 AND t - pt <= 600, (value - pv) / (t - pt), NULL),
                               value) AS r
                       FROM (
                         SELECT ts, cluster_id, node, metric, device, value, toInt64(toUnixTimestamp(ts)) AS t,
                                lagInFrame(value) OVER w AS pv, lagInFrame(toInt64(toUnixTimestamp(ts))) OVER w AS pt
                           FROM pyo_observability.node_metrics
                          WHERE team_id = {team:String} AND ts >= toDateTime({start:Int64} - 600) AND ts < toDateTime({end:Int64})
                            AND ({cluster:String} = '' OR cluster_id = {cluster:String})
                            AND metric IN ('load1', 'disk_read_bytes_total', 'disk_written_bytes_total', 'network_receive_bytes_total', 'network_transmit_bytes_total')
                         WINDOW w AS (PARTITION BY cluster_id, node, metric, device ORDER BY ts ROWS BETWEEN 1 PRECEDING AND CURRENT ROW))
                      WHERE ts >= toDateTime({start:Int64}))
                    GROUP BY b, cluster_id, node, metric, device)
                 GROUP BY b, cluster_id, node, metric"
                    .into(),
                vec![("team", team_id.to_owned()), ("cluster", cluster_id.to_owned()), ("start", start.to_string()), ("end", end.to_string()), ("step", step.to_string())],
            )
            .await?;
        let mut out: HashMap<(String, String, String), Vec<Option<f64>>> = HashMap::new();
        let first = times.first().copied().unwrap_or(0) / 1000;
        for r in &rows {
            let key = (
                r["cluster_id"].as_str().unwrap_or("").to_owned(),
                r["node"].as_str().unwrap_or("").to_owned(),
                r["metric"].as_str().unwrap_or("").to_owned(),
            );
            let b = r["b"].as_i64().unwrap_or(0);
            if b < first {
                continue;
            }
            let i = ((b - first) / step) as usize;
            let v = r["v"]
                .as_f64()
                .filter(|x| x.is_finite())
                .map(|x| (x * 100.0).round() / 100.0);
            let e = out.entry(key).or_insert_with(|| vec![None; times.len()]);
            if i < e.len() {
                e[i] = v;
            }
        }
        Ok(out)
    }

    /// Use the database at `path` from now on.
    pub fn load_geoip(&self, path: &std::path::Path) -> anyhow::Result<()> {
        // SAFETY: the file is never changed in place: geoip.rs writes a new
        // one and renames it over, so this mapping keeps the old inode.
        let r = unsafe { Reader::open_mmap(path) }?;
        *self
            .geo
            .write()
            .map_err(|_| anyhow::anyhow!("GeoIP lock poisoned"))? = Some(Arc::new(r));
        Ok(())
    }

    /// Country code, country and city of an IP address (empty when unknown).
    pub fn locate(&self, ip: &str) -> (String, String, String) {
        let none = (String::new(), String::new(), String::new());
        let geo = self.geo.read().ok().and_then(|g| g.clone());
        let (Some(geo), Ok(addr)) = (geo, ip.parse::<IpAddr>()) else {
            return none;
        };
        let Ok(found) = geo.lookup(addr) else {
            return none;
        };
        let Ok(Some(city)) = found.decode::<geoip2::City>() else {
            return none;
        };
        (
            city.country.iso_code.unwrap_or("").to_owned(),
            city.country.names.english.unwrap_or("").to_owned(),
            city.city.names.english.unwrap_or("").to_owned(),
        )
    }

    /// The web server's request log for a set of hostnames: newest first,
    /// `limit` rows before `before` (ms; now when 0). Filters: a status class
    /// ("2xx"…"5xx"), a method, and text in the host, path or URL. Client IPs
    /// follow the analytics setting, like the analytics page's.
    /// Requests, server errors and response times per answering server, for
    /// some hostnames over the last `hours`. Empty without storage.
    pub async fn per_server(
        &self,
        team_id: &str,
        hosts: &[String],
        hours: i64,
    ) -> ApiResult<Vec<Value>> {
        let Some(ch) = self.store().await else {
            return Ok(vec![]);
        };
        if hosts.is_empty() {
            return Ok(vec![]);
        }
        let hosts_param = format!(
            "[{}]",
            hosts
                .iter()
                .map(|h| format!("'{}'", h.replace(['\\', '\''], "")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let since = pyo_proto::now_ms() - hours * 3_600_000;
        Ok(ch
            .rows(
                "SELECT node, count() AS requests, countIf(status >= 500) AS errors, round(avg(duration_ms), 1) AS avgMs, round(quantile(0.95)(duration_ms), 1) AS p95Ms
                   FROM pyo_observability.requests
                  WHERE team_id = {team:String} AND has({hosts:Array(String)}, host) AND ts >= fromUnixTimestamp64Milli({since:Int64})
                  GROUP BY node"
                    .into(),
                vec![("team", team_id.to_owned()), ("hosts", hosts_param), ("since", since.to_string())],
            )
            .await?)
    }

    /// Requests per answering server per hour, for the last `hours`
    /// (sparklines). Empty without storage.
    pub async fn per_server_hourly(
        &self,
        team_id: &str,
        hosts: &[String],
        hours: i64,
    ) -> ApiResult<Vec<Value>> {
        let Some(ch) = self.store().await else {
            return Ok(vec![]);
        };
        if hosts.is_empty() {
            return Ok(vec![]);
        }
        let hosts_param = format!(
            "[{}]",
            hosts
                .iter()
                .map(|h| format!("'{}'", h.replace(['\\', '\''], "")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let since = pyo_proto::now_ms() - hours * 3_600_000;
        Ok(ch
            .rows(
                "SELECT node, toUnixTimestamp(toStartOfHour(ts)) AS hour, count() AS requests
                   FROM pyo_observability.requests
                  WHERE team_id = {team:String} AND has({hosts:Array(String)}, host) AND ts >= fromUnixTimestamp64Milli({since:Int64})
                  GROUP BY node, hour"
                    .into(),
                vec![("team", team_id.to_owned()), ("hosts", hosts_param), ("since", since.to_string())],
            )
            .await?)
    }

    pub async fn access_logs(
        &self,
        team_id: &str,
        hosts: &[String],
        f: &LogFilter,
    ) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let now = pyo_proto::now_ms();
        let since = now
            - match f.range.as_str() {
                "1h" => 3_600_000,
                "24h" => 86_400_000,
                "30d" => 30 * 86_400_000,
                _ => 7 * 86_400_000,
            };
        let before = if f.before > 0 { f.before } else { now + 1 };
        let hosts_param = format!(
            "[{}]",
            hosts
                .iter()
                .map(|h| format!("'{}'", h.replace(['\\', '\''], "")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut wh = String::from(
            "team_id = {team:String} AND has({hosts:Array(String)}, host) AND ts >= fromUnixTimestamp64Milli({since:Int64}) AND ts < fromUnixTimestamp64Milli({before:Int64})",
        );
        let mut params: Vec<(&str, String)> = vec![
            ("team", team_id.to_owned()),
            ("hosts", hosts_param),
            ("since", since.to_string()),
            ("before", before.to_string()),
        ];
        if let Some(class) = f
            .status
            .as_deref()
            .and_then(|s| s.strip_suffix("xx"))
            .and_then(|d| d.parse::<u16>().ok())
            .filter(|d| (1..=5).contains(d))
        {
            wh.push_str(" AND intDiv(status, 100) = {class:UInt16}");
            params.push(("class", class.to_string()));
        }
        if let Some(m) = f.method.as_deref().filter(|m| !m.is_empty()) {
            wh.push_str(" AND method = {method:String}");
            params.push(("method", m.to_uppercase()));
        }
        if let Some(q) = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            wh.push_str(" AND (positionCaseInsensitive(url, {q:String}) > 0 OR positionCaseInsensitive(host, {q:String}) > 0)");
            params.push(("q", q.chars().take(200).collect()));
        }
        // Totals for the filters, on the first page only (paging back keeps them).
        let summary = if f.before == 0 {
            let r = ch
                .rows(
                    format!(
                        "SELECT count() AS requests, sum(bytes) AS bytes, countIf(status >= 500) AS e5, countIf(status >= 400 AND status < 500) AS e4,
                                quantile(0.5)(duration_ms) AS p50, quantile(0.95)(duration_ms) AS p95
                           FROM pyo_observability.requests WHERE {wh}"
                    ),
                    params.clone(),
                )
                .await
                .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, format!("Couldn't read the logs: {e}")))?;
            let r = r.first().cloned().unwrap_or_default();
            let n = |k: &str| {
                r[k].as_f64()
                    .or_else(|| r[k].as_str().and_then(|s| s.parse().ok()))
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0)
            };
            Some(
                json!({ "requests": n("requests"), "bytes": n("bytes"), "serverErrors": n("e5"), "clientErrors": n("e4"), "p50": n("p50").round(), "p95": n("p95").round() }),
            )
        } else {
            None
        };
        let limit = f.limit.clamp(1, 500);
        params.push(("limit", (limit + 1).to_string()));
        let rows = ch
            .rows(
                format!(
                    "SELECT toUnixTimestamp64Milli(ts) AS at, host, method, path, url, status, duration_ms, bytes, country, country_name, city, referrer, device, browser, ip, user_agent
                       FROM pyo_observability.requests WHERE {wh} ORDER BY ts DESC LIMIT {{limit:UInt32}}"
                ),
                params,
            )
            .await
            .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, format!("Couldn't read the logs: {e}")))?;
        let more = rows.len() > limit as usize;
        let mode = self.current_ip_mode().await;
        let logs: Vec<Value> = rows
            .iter()
            .take(limit as usize)
            .map(|r| {
                json!({
                    "at": r["at"], "host": r["host"], "method": r["method"], "path": r["path"], "url": r["url"],
                    "status": r["status"], "durationMs": r["duration_ms"].as_f64().unwrap_or(0.0).round(), "bytes": r["bytes"],
                    "country": r["country"], "countryName": r["country_name"], "city": r["city"], "referrer": r["referrer"], "device": r["device"], "browser": r["browser"], "userAgent": r["user_agent"],
                    "ip": mode.apply(r["ip"].as_str().unwrap_or("")),
                })
            })
            .collect();
        let next = if more {
            logs.last().and_then(|l| l["at"].as_i64())
        } else {
            None
        };
        Ok(json!({ "logs": logs, "next": next, "ipMode": mode, "summary": summary }))
    }

    /// The analytics page's data for a set of hostnames of one team.
    pub async fn query(&self, team_id: &str, hosts: &[String], range: &str) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let (points, step) = match range {
            "1h" => (60i64, 60i64),
            "24h" => (48, 1800),
            "30d" => (30, 86400),
            _ => (42, 14400),
        };
        let now = pyo_proto::now_ms() / 1000;
        let end = (now / step + 1) * step;
        let start = end - points * step;
        let prev_start = start - points * step;

        let hosts_param = format!(
            "[{}]",
            hosts
                .iter()
                .map(|h| format!("'{}'", h.replace(['\\', '\''], "")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let p = |s: i64, e: i64| -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("hosts", hosts_param.clone()),
                ("start", s.to_string()),
                ("end", e.to_string()),
                ("step", step.to_string()),
            ]
        };
        const W: &str = "team_id = {team:String} AND has({hosts:Array(String)}, host) AND ts >= toDateTime64({start:Int64}, 3) AND ts < toDateTime64({end:Int64}, 3)";

        let summary_sql = format!(
            "SELECT count() AS requests, uniq(visitor) AS visitors, countIf(pageview = 1) AS pageviews,
                    sum(bytes) / 1e6 AS mb, countIf(status >= 500) AS errors,
                    quantiles(0.5, 0.95, 0.99)(duration_ms) AS q
               FROM pyo_observability.requests WHERE {W}"
        );
        let (cur, prev, series, statuses, pages, paths, refs, countries, cities, devices, browsers, params, recent, ips) = tokio::try_join!(
            ch.rows(summary_sql.clone(), p(start, end)),
            ch.rows(summary_sql.clone(), p(prev_start, start)),
            ch.rows(
                format!(
                    "SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, count() AS requests, uniq(visitor) AS visitors,
                            sum(bytes) / 1e6 AS mb, countIf(status >= 500) AS errors, quantiles(0.5, 0.95, 0.99)(duration_ms) AS q
                       FROM pyo_observability.requests WHERE {W} GROUP BY b ORDER BY b"
                ),
                p(start, end)
            ),
            ch.rows(format!("SELECT concat(toString(intDiv(status, 100)), 'xx') AS code, count() AS c FROM pyo_observability.requests WHERE {W} AND status >= 200 GROUP BY code"), p(start, end)),
            ch.rows(format!("SELECT url, any(path) AS path, count() AS views, uniq(visitor) AS visitors, avg(duration_ms) AS ms FROM pyo_observability.requests WHERE {W} AND pageview = 1 GROUP BY url ORDER BY views DESC LIMIT 100"), p(start, end)),
            ch.rows(format!("SELECT path, count() AS c, quantile(0.95)(duration_ms) AS p95 FROM pyo_observability.requests WHERE {W} GROUP BY path ORDER BY c DESC LIMIT 25"), p(start, end)),
            ch.rows(format!("SELECT referrer AS k, uniq(visitor) AS v FROM pyo_observability.requests WHERE {W} AND pageview = 1 AND referrer != '' GROUP BY k ORDER BY v DESC LIMIT 25"), p(start, end)),
            ch.rows(format!("SELECT if(country = '', '??', country) AS code, if(country_name = '', 'Unknown', country_name) AS name, uniq(visitor) AS v FROM pyo_observability.requests WHERE {W} GROUP BY code, name ORDER BY v DESC LIMIT 25"), p(start, end)),
            ch.rows(format!("SELECT city, any(country) AS country, uniq(visitor) AS v FROM pyo_observability.requests WHERE {W} AND city != '' GROUP BY city ORDER BY v DESC LIMIT 25"), p(start, end)),
            ch.rows(format!("SELECT device AS k, uniq(visitor) AS v FROM pyo_observability.requests WHERE {W} GROUP BY k ORDER BY v DESC"), p(start, end)),
            ch.rows(format!("SELECT browser AS k, uniq(visitor) AS v FROM pyo_observability.requests WHERE {W} GROUP BY k ORDER BY v DESC LIMIT 15"), p(start, end)),
            ch.rows(
                format!(
                    "SELECT splitByChar('=', kv)[1] AS k, count() AS c, uniqExact(kv) AS d, max(position(kv, '[redacted]') > 0) AS r
                       FROM pyo_observability.requests ARRAY JOIN extractURLParameters(url) AS kv WHERE {W} GROUP BY k ORDER BY c DESC LIMIT 30"
                ),
                p(start, end)
            ),
            ch.rows(format!("SELECT toUnixTimestamp64Milli(ts) AS at, method, url, path, status, duration_ms, bytes, country, ip FROM pyo_observability.requests WHERE {W} ORDER BY ts DESC LIMIT 50"), p(start, end)),
            ch.rows(format!("SELECT ip AS k, count() AS c, uniq(visitor) AS v FROM pyo_observability.requests WHERE {W} AND ip != '' GROUP BY k ORDER BY c DESC LIMIT 100"), p(start, end)),
        )?;
        // Client IPs as the analytics setting says now: hidden when it's
        // "none", shortened when it's "anonymized" (for rows kept in full
        // before the setting changed).
        let mode = self.current_ip_mode().await;
        let mut by_ip: Vec<(String, f64, f64)> = vec![];
        for r in &ips {
            let ip = mode.apply(r["k"].as_str().unwrap_or(""));
            if ip.is_empty() {
                continue;
            }
            let (c, v) = (
                r["c"].as_f64().unwrap_or(0.0),
                r["v"].as_f64().unwrap_or(0.0),
            );
            match by_ip.iter_mut().find(|x| x.0 == ip) {
                Some(x) => {
                    x.1 += c;
                    x.2 += v;
                }
                None => by_ip.push((ip, c, v)),
            }
        }
        by_ip.sort_by(|a, b| b.1.total_cmp(&a.1));
        by_ip.truncate(25);

        let num = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        let q = |v: &Value, i: usize| {
            v["q"][i]
                .as_f64()
                .filter(|x| x.is_finite())
                .unwrap_or(0.0)
                .round()
        };
        let c = cur.first().cloned().unwrap_or_default();
        let pv = prev.first().cloned().unwrap_or_default();
        let requests = num(&c, "requests");

        // Every bucket, including empty ones.
        let by_bucket: HashMap<i64, &Value> = series
            .iter()
            .map(|r| (r["b"].as_i64().unwrap_or(0), r))
            .collect();
        let series: Vec<Value> = (0..points)
            .map(|i| {
                let b = start + i * step;
                let r = by_bucket.get(&b);
                let g = |k: &str| r.map(|r| num(r, k)).unwrap_or(0.0);
                json!({
                    "at": b * 1000,
                    "requests": g("requests"), "visitors": g("visitors"), "bandwidthMb": g("mb"), "errors": g("errors"),
                    "p50": r.map(|r| q(r, 0)).unwrap_or(0.0), "p95": r.map(|r| q(r, 1)).unwrap_or(0.0), "p99": r.map(|r| q(r, 2)).unwrap_or(0.0),
                })
            })
            .collect();

        let status_codes: Vec<Value> = ["2xx", "3xx", "4xx", "5xx"]
            .iter()
            .map(|code| json!({ "code": code, "count": statuses.iter().find(|s| s["code"] == json!(code)).map(|s| num(s, "c")).unwrap_or(0.0) }))
            .collect();
        let kv = |rows: &[Value], key: &str, label: &str| -> Vec<Value> {
            rows.iter()
                .map(|r| json!({ key: r["k"], label: num(r, "v") }))
                .collect()
        };
        let summary = json!({
            "range": range,
            "requests": requests,
            "estimatedVisitors": num(&c, "visitors"),
            "pageviews": num(&c, "pageviews"),
            "bandwidthMb": num(&c, "mb"),
            "errorRate": if requests > 0.0 { num(&c, "errors") / requests } else { 0.0 },
            "latency": { "p50": q(&c, 0), "p95": q(&c, 1), "p99": q(&c, 2) },
            "statusCodes": status_codes,
            "topPaths": paths.iter().map(|r| json!({ "path": r["path"], "requests": num(r, "c"), "p95": num(r, "p95").round() })).collect::<Vec<_>>(),
            "referrers": kv(&refs, "source", "visitors"),
            "countries": countries.iter().map(|r| json!({ "code": r["code"], "name": r["name"], "visitors": num(r, "v") })).collect::<Vec<_>>(),
            "cities": cities.iter().map(|r| json!({ "city": r["city"], "country": r["country"], "visitors": num(r, "v") })).collect::<Vec<_>>(),
            "pages": pages.iter().map(|r| json!({ "url": r["url"], "path": r["path"], "views": num(r, "views"), "visitors": num(r, "visitors"), "avgMs": num(r, "ms").round() })).collect::<Vec<_>>(),
            "devices": kv(&devices, "label", "visitors"),
            "browsers": kv(&browsers, "label", "visitors"),
            "queryParams": params.iter().map(|r| json!({ "key": r["k"], "requests": num(r, "c"), "distinct": num(r, "d"), "redacted": num(r, "r") > 0.0 })).collect::<Vec<_>>(),
            "recentRequests": recent.iter().enumerate().map(|(i, r)| json!({ "id": format!("req_{i}"), "at": r["at"], "method": r["method"], "url": r["url"], "path": r["path"], "status": r["status"], "durationMs": num(r, "duration_ms").round(), "bytes": r["bytes"], "country": r["country"], "ip": mode.apply(r["ip"].as_str().unwrap_or("")) })).collect::<Vec<_>>(),
            "ipMode": mode,
            "clientIps": by_ip.iter().map(|(ip, c, v)| json!({ "ip": ip, "requests": c, "visitors": v })).collect::<Vec<_>>(),
        });
        let prev_requests = num(&pv, "requests");
        Ok(json!({
            "summary": summary,
            "series": series,
            "previous": {
                "estimatedVisitors": num(&pv, "visitors"),
                "pageviews": num(&pv, "pageviews"),
                "errorRate": if prev_requests > 0.0 { num(&pv, "errors") / prev_requests } else { 0.0 },
            },
        }))
    }
}

impl Observability {
    /// A service's connections to its public TCP and UDP ports through the
    /// gateway: per port, over time, by country and outcome, and the
    /// latest ones.
    pub async fn connections(
        &self,
        team_id: &str,
        service_id: &str,
        range: &str,
    ) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let (points, step) = match range {
            "1h" => (60i64, 60i64),
            "24h" => (48, 1800),
            "30d" => (30, 86400),
            _ => (42, 14400),
        };
        let now = pyo_proto::now_ms() / 1000;
        let end = (now / step + 1) * step;
        let start = end - points * step;
        let p = || -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("service", service_id.to_owned()),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("step", step.to_string()),
            ]
        };
        const W: &str = "team_id = {team:String} AND service_id = {service:String} AND ts >= toDateTime64({start:Int64}, 3) AND ts < toDateTime64({end:Int64}, 3)";
        let (ports, series, countries, outcomes, recent) = tokio::try_join!(
            ch.rows(format!("SELECT port, protocol, count() AS c, uniq(visitor) AS v, sum(bytes_in) AS bin, sum(bytes_out) AS bout, quantiles(0.5, 0.95)(duration_ms) AS q, countIf(outcome = 'blocked') AS blocked, countIf(outcome = 'limited') AS limited, countIf(outcome = 'unreachable') AS unreachable FROM pyo_observability.connections WHERE {W} GROUP BY port, protocol ORDER BY port"), p()),
            ch.rows(format!("SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, count() AS c, uniq(visitor) AS v, sum(bytes_in + bytes_out) AS bytes FROM pyo_observability.connections WHERE {W} GROUP BY b ORDER BY b"), p()),
            ch.rows(format!("SELECT if(country = '', '??', country) AS code, if(country_name = '', 'Unknown', country_name) AS name, uniq(visitor) AS v, count() AS c FROM pyo_observability.connections WHERE {W} GROUP BY code, name ORDER BY c DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT outcome AS k, count() AS c FROM pyo_observability.connections WHERE {W} GROUP BY k"), p()),
            ch.rows(format!("SELECT toUnixTimestamp64Milli(ts) AS at, port, protocol, ip, country, duration_ms, bytes_in, bytes_out, outcome FROM pyo_observability.connections WHERE {W} ORDER BY ts DESC LIMIT 50"), p()),
        )?;
        let mode = self.current_ip_mode().await;
        let num = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        let q = |v: &Value, i: usize| {
            v["q"][i]
                .as_f64()
                .filter(|x| x.is_finite())
                .unwrap_or(0.0)
                .round()
        };
        let by_bucket: HashMap<i64, &Value> = series
            .iter()
            .map(|r| (r["b"].as_i64().unwrap_or(0), r))
            .collect();
        let series: Vec<Value> = (0..points)
            .map(|i| {
                let b = start + i * step;
                let r = by_bucket.get(&b);
                let g = |k: &str| r.map(|r| num(r, k)).unwrap_or(0.0);
                json!({ "at": b * 1000, "connections": g("c"), "clients": g("v"), "bytes": g("bytes") })
            })
            .collect();
        Ok(json!({
            "range": range,
            "ports": ports.iter().map(|r| json!({
                "port": r["port"], "protocol": r["protocol"], "connections": num(r, "c"), "clients": num(r, "v"),
                "bytesIn": num(r, "bin"), "bytesOut": num(r, "bout"), "durationP50Ms": q(r, 0), "durationP95Ms": q(r, 1),
                "blocked": num(r, "blocked"), "limited": num(r, "limited"), "unreachable": num(r, "unreachable"),
            })).collect::<Vec<_>>(),
            "series": series,
            "countries": countries.iter().map(|r| json!({ "code": r["code"], "name": r["name"], "clients": num(r, "v"), "connections": num(r, "c") })).collect::<Vec<_>>(),
            "outcomes": outcomes.iter().map(|r| json!({ "outcome": r["k"], "count": num(r, "c") })).collect::<Vec<_>>(),
            "recent": recent.iter().map(|r| json!({
                "at": r["at"], "port": r["port"], "protocol": r["protocol"], "ip": mode.apply(r["ip"].as_str().unwrap_or("")), "country": r["country"],
                "durationMs": num(r, "duration_ms").round(), "bytesIn": r["bytes_in"], "bytesOut": r["bytes_out"], "outcome": r["outcome"],
            })).collect::<Vec<_>>(),
            "ipMode": mode,
        }))
    }
}

impl Observability {
    /// What the firewall turned away for these services: over time, per
    /// rule, source, country and website path, and the latest, from
    /// websites (403s and 429s the proxy answered itself) and TCP ports
    /// through the proxy.
    pub async fn blocked(
        &self,
        team_id: &str,
        services: &[String],
        range: &str,
    ) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let secs = match range {
            "1h" => 3600,
            "7d" => 7 * 86400,
            "30d" => 30 * 86400,
            _ => 86400,
        };
        let start = pyo_proto::now_ms() / 1000 - secs;
        let services_param = format!(
            "[{}]",
            services
                .iter()
                .map(|a| format!("'{}'", a.replace(['\\', '\''], "")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let step = bar_steps(range).1;
        let p = || -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("services", services_param.clone()),
                ("start", start.to_string()),
                ("step", step.to_string()),
            ]
        };
        // Both tables, as one: a website request or a connection.
        let both = "(SELECT ts, service_id, 'website' AS kind, host AS target, path, ip, country, country_name, blocked_by AS rule, visitor FROM pyo_observability.requests
                      WHERE team_id = {team:String} AND has({services:Array(String)}, service_id) AND ts >= toDateTime64({start:Int64}, 3) AND blocked_by != ''
                     UNION ALL
                     SELECT ts, service_id, lower(protocol) AS kind, toString(port) AS target, '' AS path, ip, country, country_name, rule, visitor FROM pyo_observability.connections
                      WHERE team_id = {team:String} AND has({services:Array(String)}, service_id) AND ts >= toDateTime64({start:Int64}, 3) AND outcome = 'blocked')";
        let (rules, sources, countries, paths, series, recent) = tokio::try_join!(
            ch.rows(format!("SELECT rule, count() AS c FROM {both} GROUP BY rule ORDER BY c DESC LIMIT 50"), p()),
            ch.rows(format!("SELECT ip, any(country) AS country, any(country_name) AS name, count() AS c, uniq(visitor) AS v FROM {both} GROUP BY ip ORDER BY c DESC LIMIT 50"), p()),
            ch.rows(format!("SELECT country, count() AS c FROM {both} GROUP BY country ORDER BY c DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT path, count() AS c FROM {both} WHERE kind = 'website' GROUP BY path ORDER BY c DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, count() AS c FROM {both} GROUP BY b"), p()),
            ch.rows(format!("SELECT toUnixTimestamp64Milli(ts) AS at, service_id, kind, target, ip, country, rule FROM {both} ORDER BY ts DESC LIMIT 50"), p()),
        )?;
        let mode = self.current_ip_mode().await;
        let num = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        Ok(json!({
            "range": range,
            "total": rules.iter().map(|r| num(r, "c")).sum::<f64>(),
            "series": bar_series(&series, range, |r| json!({ "count": r.map(|r| num(r, "c")).unwrap_or(0.0) })),
            "rules": rules.iter().map(|r| json!({ "rule": r["rule"], "count": num(r, "c") })).collect::<Vec<_>>(),
            "sources": sources.iter().map(|r| json!({ "ip": mode.apply(r["ip"].as_str().unwrap_or("")), "country": r["country"], "countryName": r["name"], "count": num(r, "c"), "clients": num(r, "v") })).collect::<Vec<_>>(),
            "countries": countries.iter().map(|r| json!({ "country": r["country"], "count": num(r, "c") })).collect::<Vec<_>>(),
            "paths": paths.iter().map(|r| json!({ "path": r["path"], "count": num(r, "c") })).collect::<Vec<_>>(),
            "recent": recent.iter().map(|r| json!({ "at": r["at"], "serviceId": r["service_id"], "kind": r["kind"], "target": r["target"], "ip": mode.apply(r["ip"].as_str().unwrap_or("")), "country": r["country"], "rule": r["rule"] })).collect::<Vec<_>>(),
            "ipMode": mode,
        }))
    }

    /// The WAF's findings for these services: blocked and logged
    /// requests, over time, per rule, domain, source, country and path, the
    /// latest 50, and the custom rules' match counts.
    pub async fn waf_hits(
        &self,
        team_id: &str,
        services: &[String],
        range: &str,
    ) -> ApiResult<Value> {
        let ch = self.reader().await?;
        let secs = match range {
            "1h" => 3600,
            "7d" => 7 * 86400,
            "30d" => 30 * 86400,
            _ => 86400,
        };
        let start = pyo_proto::now_ms() / 1000 - secs;
        let services_param = format!(
            "[{}]",
            services
                .iter()
                .map(|a| format!("'{}'", a.replace(['\\', '\''], "")))
                .collect::<Vec<_>>()
                .join(",")
        );
        let step = bar_steps(range).1;
        let p = || -> Vec<(&'static str, String)> {
            vec![
                ("team", team_id.to_owned()),
                ("services", services_param.clone()),
                ("start", start.to_string()),
                ("step", step.to_string()),
            ]
        };
        const W: &str = "team_id = {team:String} AND has({services:Array(String)}, service_id) AND ts >= toDateTime64({start:Int64}, 3)";
        // Custom rules are numbered from 1000000 (pyo_proto::WAF_RULE_IDS);
        // the Core Rule Set's from 949000 are its scoring, not findings.
        let (totals, rules, hosts, recent, custom, (series, sources, countries, paths)) = tokio::try_join!(
            ch.rows(format!("SELECT countIf(blocked = 1) AS nb, countIf(blocked = 0) AS nl, uniq(visitor) AS v FROM pyo_observability.waf_hits WHERE {W}"), p()),
            ch.rows(format!("SELECT rule, any(msg) AS message, count() AS c FROM pyo_observability.waf_hits ARRAY JOIN rules AS rule, messages AS msg WHERE {W} AND (rule < 949000 OR rule >= 1000000) GROUP BY rule ORDER BY c DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT host, countIf(blocked = 1) AS nb, countIf(blocked = 0) AS nl FROM pyo_observability.waf_hits WHERE {W} GROUP BY host ORDER BY nb + nl DESC LIMIT 25"), p()),
            ch.rows(format!("SELECT toUnixTimestamp64Milli(ts) AS at, service_id, host, method, url, ip, country, blocked, rules, messages, severity FROM pyo_observability.waf_hits WHERE {W} ORDER BY ts DESC LIMIT 50"), p()),
            ch.rows(format!("SELECT rule, count() AS c FROM pyo_observability.waf_hits ARRAY JOIN rules AS rule WHERE {W} AND rule >= 1000000 GROUP BY rule"), p()),
            async {
                tokio::try_join!(
                    ch.rows(format!("SELECT toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:Int64}}))) AS b, countIf(blocked = 1) AS nb, countIf(blocked = 0) AS nl FROM pyo_observability.waf_hits WHERE {W} GROUP BY b"), p()),
                    ch.rows(format!("SELECT ip, any(country) AS country, countIf(blocked = 1) AS nb, countIf(blocked = 0) AS nl FROM pyo_observability.waf_hits WHERE {W} GROUP BY ip ORDER BY nb + nl DESC LIMIT 50"), p()),
                    ch.rows(format!("SELECT country, countIf(blocked = 1) AS nb, countIf(blocked = 0) AS nl FROM pyo_observability.waf_hits WHERE {W} GROUP BY country ORDER BY nb + nl DESC LIMIT 25"), p()),
                    ch.rows(format!("SELECT path, countIf(blocked = 1) AS nb, countIf(blocked = 0) AS nl FROM pyo_observability.waf_hits WHERE {W} GROUP BY path ORDER BY nb + nl DESC LIMIT 25"), p()),
                )
            },
        )?;
        let mode = self.current_ip_mode().await;
        let num = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        let t = totals.first().cloned().unwrap_or_default();
        Ok(json!({
            "range": range,
            "blocked": num(&t, "nb"), "logged": num(&t, "nl"), "clients": num(&t, "v"),
            "rules": rules.iter().map(|r| json!({ "rule": r["rule"], "message": r["message"], "count": num(r, "c") })).collect::<Vec<_>>(),
            "hosts": hosts.iter().map(|r| json!({ "host": r["host"], "blocked": num(r, "nb"), "logged": num(r, "nl") })).collect::<Vec<_>>(),
            "series": bar_series(&series, range, |r| json!({ "blocked": r.map(|r| num(r, "nb")).unwrap_or(0.0), "logged": r.map(|r| num(r, "nl")).unwrap_or(0.0) })),
            "sources": sources.iter().map(|r| json!({ "ip": mode.apply(r["ip"].as_str().unwrap_or("")), "country": r["country"], "blocked": num(r, "nb"), "logged": num(r, "nl") })).collect::<Vec<_>>(),
            "countries": countries.iter().map(|r| json!({ "country": r["country"], "blocked": num(r, "nb"), "logged": num(r, "nl") })).collect::<Vec<_>>(),
            "paths": paths.iter().map(|r| json!({ "path": r["path"], "blocked": num(r, "nb"), "logged": num(r, "nl") })).collect::<Vec<_>>(),
            // Each custom rule's matches in the range, by its number.
            "custom": custom.iter().map(|r| json!({ "rule": r["rule"], "count": num(r, "c") })).collect::<Vec<_>>(),
            "recent": recent.iter().map(|r| json!({
                "at": r["at"], "serviceId": r["service_id"], "host": r["host"], "method": r["method"], "url": r["url"],
                "ip": mode.apply(r["ip"].as_str().unwrap_or("")), "country": r["country"], "blocked": num(r, "blocked") > 0.0,
                "rules": r["rules"], "messages": r["messages"], "severity": r["severity"],
            })).collect::<Vec<_>>(),
            "ipMode": mode,
        }))
    }

    /// An address's country, for the dashboard's lockout check.
    pub fn country_of(&self, ip: &str) -> String {
        self.locate(ip).0
    }
}

/// "/a?token=x&page=2" -> ("/a", "/a?token=[redacted]&page=2", ["token","page"], ["token"]).
fn split_url(raw: &str) -> (String, String, Vec<String>, Vec<String>) {
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
            if SENSITIVE.iter().any(|s| lower.contains(s)) {
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
    (
        path.clone(),
        format!("{path}?{}", parts.join("&")),
        keys,
        redacted,
    )
}

#[cfg(test)]
mod tests {
    use super::{IpMode, split_url};

    /// The proxy's access log, as Vector sends it: every field analytics uses
    /// comes from a field the proxy writes.
    #[test]
    fn access_log_fields() {
        use super::AccessLine;
        use serde_json::json;
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
        let mut written: Vec<&str> = pyo_proto::system::ACCESS_LOG_FIELDS
            .iter()
            .map(|(k, _)| *k)
            .collect();
        written.push(pyo_proto::system::ACCESS_LOG_CONTENT_TYPE.0);
        written.sort();
        let mut read = vec![
            "host",
            "start_time",
            "peer",
            "x_forwarded_for",
            "x_real_ip",
            "user_agent",
            "status",
            "method",
            "path",
            "content_type",
            "referer",
            "duration_ms",
            "bytes",
            "remote",
            "local_port",
            "bytes_in",
            "upstream",
            "upstream_host",
            "flags",
            "termination",
            "details",
        ];
        read.sort();
        assert_eq!(written, read);
    }

    #[test]
    fn connection_lines() {
        use super::AccessLine;
        use serde_json::json;
        // Envoy's line for a TCP connection: no HTTP fields.
        let tcp = AccessLine::read(
            &json!({ "start_time": "2026-11-06T12:28:32.253Z", "peer": "10.0.2.2", "remote": "203.0.113.9", "local_port": "16379", "bytes": "120", "bytes_in": 64,
            "duration_ms": 5012, "upstream": "tcproute/pyo-gateway/shop-cache-tcp-6379/rule/0", "flags": "-", "termination": null, "host": null, "method": null }),
        );
        assert_eq!(
            tcp.port_key(),
            Some(("TCP", super::PortKey::Route("shop-cache-tcp-6379")))
        );
        assert_eq!(
            (
                tcp.bytes_in,
                tcp.bytes,
                tcp.duration_ms,
                tcp.remote.as_str(),
                tcp.outcome()
            ),
            (64, 120, 5012.0, "203.0.113.9", "ok")
        );
        // As Envoy Gateway v1.9.3 logs them: a UDP session (no route named,
        // after a minute without packets), and a TCP connection the limit
        // turned away (nothing upstream).
        let udp = AccessLine::read(
            &json!({ "bytes": 123, "bytes_in": 55, "duration_ms": 60004, "flags": "-", "host": null, "local_port": 10053, "method": null, "peer": "10.0.2.2", "remote": "10.0.2.2", "status": 0, "upstream": null, "upstream_host": "10.43.152.148:53" }),
        );
        assert_eq!(
            (udp.port_key(), udp.outcome()),
            (Some(("UDP", super::PortKey::Listen(10053))), "ok")
        );
        let limited = AccessLine::read(
            &json!({ "bytes": 0, "bytes_in": 0, "duration_ms": 0, "flags": "-", "local_port": 6379, "peer": "10.0.2.2", "upstream": null, "upstream_host": null }),
        );
        assert_eq!(
            (limited.port_key(), limited.outcome()),
            (Some(("TCP", super::PortKey::Listen(6379))), "limited")
        );
        let outcome = |flags: &str, termination: &str| {
            AccessLine {
                upstream: "tcproute/x/y/rule/0".into(),
                flags: flags.into(),
                termination: termination.into(),
                ..Default::default()
            }
            .outcome()
        };
        assert_eq!(
            outcome("-", "rbac_access_denied_matched_policy[deny]"),
            "blocked"
        );
        assert_eq!(outcome("UF,URX", ""), "unreachable");
        assert_eq!(outcome("UO", ""), "limited");
    }

    #[test]
    fn the_traffic_proxy_is_measured_against_the_whole_cluster() {
        use serde_json::{Value, json};
        let workload = |name: &str, ns: &str| json!({ "name": name, "namespace": ns, "kind": "DaemonSet", "status": "healthy", "desired": 2, "ready": 2, "cpuMillis": 250.0, "memoryMi": 96.0, "serviceId": null, "system": true, "servers": [], "createdAt": 0 });
        let node = |cpu: f64, gi: f64| json!({ "name": "n", "roles": [], "status": "ready", "schedulable": true, "maintenance": false, "kubeletVersion": "", "osImage": "", "arch": "", "containerRuntime": "", "internalIP": "", "externalIP": null, "capacity": { "cpu": cpu, "memoryGi": gi, "pods": 110 }, "usage": { "cpu": 0.0, "memoryGi": 0.0 }, "podCount": 0, "conditions": [], "createdAt": 0 });
        let heartbeat = |workloads: Value| -> pyo_proto::Heartbeat {
            serde_json::from_value(json!({
                "protocol": 1, "agentVersion": "", "kubernetesVersion": "", "services": [], "volumes": [],
                "servers": [node(4.0, 8.0), node(2.0, 4.0)], "workloads": workloads,
                "capabilities": { "ingressController": "envoy", "loadBalancer": "servicelb", "loadBalancerIps": [], "certManager": false, "metricsServer": true, "storageClasses": [], "podCidr": "", "serviceCidr": "" },
            }))
            .unwrap()
        };
        // No proxy yet; someone's own service called envoy-x isn't it.
        assert!(
            super::ingress_sample(&heartbeat(json!([
                workload("web", "shop"),
                workload("envoy-x", "shop")
            ])))
            .is_none()
        );
        // Envoy Gateway: the controller and the proxies together; pods are proxies.
        let mut controller = workload("envoy-gateway", "pyo-gateway");
        controller["kind"] = json!("Deployment");
        controller["ready"] = json!(1);
        let t = super::ingress_sample(&heartbeat(json!([
            workload("web", "shop"),
            controller,
            workload("envoy-pyo-695a31a2", "pyo-gateway")
        ])))
        .unwrap();
        assert_eq!(
            (
                t.cpu,
                t.cpu_capacity,
                t.memory_mi,
                t.memory_capacity_mi,
                t.pods
            ),
            (0.5, 6.0, 192.0, 12288.0, 2)
        );
    }

    #[test]
    fn visitors_are_who_the_proxy_saw() {
        let line = |peer: &str, remote: &str, xff: &str, real: &str| super::AccessLine {
            peer: peer.into(),
            remote: remote.into(),
            forwarded_for: xff.into(),
            real_ip: real.into(),
            ..Default::default()
        };
        // Straight from the internet: the peer, whatever headers say.
        assert_eq!(
            line("203.0.113.7", "203.0.113.7", "1.2.3.4", "").visitor_ip(),
            "203.0.113.7"
        );
        // A tailnet device (or anything on a private network) naming someone
        // else: still itself, as the firewall sees it.
        assert_eq!(
            line(
                "100.101.102.103",
                "100.101.102.103",
                "198.51.100.9",
                "198.51.100.10"
            )
            .visitor_ip(),
            "100.101.102.103"
        );
        assert_eq!(
            line("10.42.0.1", "10.42.0.1", "198.51.100.9", "").visitor_ip(),
            "10.42.0.1"
        );
        // A PROXY protocol header's address, when the proxy read one.
        assert_eq!(
            line("10.0.2.2", "198.51.100.9", "6.6.6.6", "").visitor_ip(),
            "198.51.100.9"
        );
        // Lines without the proxy's view: the peer.
        assert_eq!(
            line("203.0.113.7", "", "6.6.6.6", "").visitor_ip(),
            "203.0.113.7"
        );
    }

    #[test]
    fn ip_modes() {
        assert_eq!(IpMode::None.apply("203.0.113.7"), "");
        assert_eq!(IpMode::Anonymized.apply("203.0.113.7"), "203.0.113.0");
        assert_eq!(
            IpMode::Anonymized.apply("2001:db8:85a3:1:2:3:4:5"),
            "2001:db8:85a3::"
        );
        assert_eq!(IpMode::Full.apply("203.0.113.7"), "203.0.113.7");
        assert_eq!(IpMode::Full.apply("not-an-ip"), "");
    }

    #[test]
    fn redacts_sensitive_values() {
        let (path, url, keys, redacted) = split_url("/reset?email=a@b.c&token=xyz&page=2");
        assert_eq!(path, "/reset");
        assert_eq!(url, "/reset?email=[redacted]&token=[redacted]&page=2");
        assert_eq!(keys, vec!["email", "token", "page"]);
        assert_eq!(redacted, vec!["email", "token"]);
        assert_eq!(
            split_url("/signup?invite=abc").1,
            "/signup?invite=[redacted]"
        );
    }

    #[test]
    fn plain_paths() {
        assert_eq!(split_url("/").1, "/");
        assert_eq!(split_url("/a?x").1, "/a?x");
    }

    #[test]
    fn host_metrics_kept_and_dropped() {
        use super::host_metric_row;
        use serde_json::json;
        let ev = |name: &str, tags: serde_json::Value, v: f64| json!({ "name": name, "namespace": "host", "tags": tags, "timestamp": "2026-11-06T10:00:00Z", "kind": "absolute", "counter": { "value": v } });
        let row = host_metric_row(
            "team",
            "cls",
            &ev(
                "network_receive_bytes_total",
                json!({ "node": "sg-a", "device": "eth0" }),
                1e6,
            ),
        )
        .expect("a real NIC");
        assert_eq!(
            (
                row["node"].as_str(),
                row["metric"].as_str(),
                row["device"].as_str(),
                row["value"].as_f64()
            ),
            (
                Some("sg-a"),
                Some("network_receive_bytes_total"),
                Some("eth0"),
                Some(1e6)
            )
        );
        assert_eq!(row["ts"].as_i64(), Some(1_793_959_200));
        assert!(
            host_metric_row(
                "team",
                "cls",
                &ev(
                    "network_receive_bytes_total",
                    json!({ "node": "sg-a", "device": "veth12ab" }),
                    1.0
                )
            )
            .is_none(),
            "pod veths"
        );
        assert!(
            host_metric_row(
                "team",
                "cls",
                &ev(
                    "network_receive_bytes_total",
                    json!({ "node": "sg-a", "device": "flannel.1" }),
                    1.0
                )
            )
            .is_none()
        );
        assert!(
            host_metric_row(
                "team",
                "cls",
                &ev(
                    "disk_read_bytes_total",
                    json!({ "node": "sg-a", "device": "loop3" }),
                    1.0
                )
            )
            .is_none(),
            "loop devices"
        );
        assert!(
            host_metric_row(
                "team",
                "cls",
                &ev(
                    "disk_read_bytes_total",
                    json!({ "node": "sg-a", "device": "nvme0n1" }),
                    1.0
                )
            )
            .is_some()
        );
        let fs = |mount: &str, kind: &str| {
            host_metric_row(
                "team",
                "cls",
                &json!({ "name": "filesystem_used_bytes", "tags": { "node": "sg-a", "mountpoint": mount, "filesystem": kind }, "gauge": { "value": 5.0 } }),
            )
        };
        assert!(fs("/", "ext4").is_some());
        assert!(fs("/data", "xfs").is_some());
        assert!(
            fs("/var/lib/kubelet/pods/x/volumes", "ext4").is_none(),
            "pod volume mounts"
        );
        assert!(fs("/run/k3s/containerd/x", "overlay").is_none());
        assert!(fs("/dev/shm", "tmpfs").is_none());
        assert!(
            host_metric_row(
                "team",
                "cls",
                &ev(
                    "cpu_seconds_total",
                    json!({ "node": "sg-a", "cpu": "0", "mode": "idle" }),
                    1.0
                )
            )
            .is_none(),
            "CPU comes from the heartbeat"
        );
        assert!(host_metric_row("team", "cls", &json!({ "name": "host_load1", "tags": { "node": "sg-a" }, "gauge": { "value": 0.5 } })).is_some(), "namespaced name");
        assert!(
            host_metric_row(
                "team",
                "cls",
                &json!({ "name": "load1", "tags": {}, "gauge": { "value": 0.5 } })
            )
            .is_none(),
            "no node"
        );
    }

    #[test]
    fn database_metric_rows_belong_to_their_database() {
        use pyo_proto::DatabaseMemberMetrics;
        let dbs = vec![(
            "db_1".to_owned(),
            "team_a".to_owned(),
            "pyo-db-a".to_owned(),
            "pg".to_owned(),
        )];
        let m = |ns: &str, db: &str| DatabaseMemberMetrics {
            namespace: ns.into(),
            database: db.into(),
            member: "pg-1".into(),
            component: "instance".into(),
            role: "primary".into(),
            server: "s1".into(),
            cpu: Some(0.25),
            memory_mi: None,
            disk_used_gi: Some(1.5),
            disk_capacity_gi: Some(10.0),
            values: [
                ("transactions".to_owned(), 12.5),
                ("made_up".to_owned(), 1.0),
                ("connections".to_owned(), f64::NAN),
            ]
            .into(),
        };
        let out = super::database_metric_rows(
            1_700_000_000,
            "cls",
            &dbs,
            &[m("pyo-db-a", "pg"), m("pyo-db-b", "pg")],
        );
        let rows: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let metrics: Vec<&str> = rows.iter().map(|r| r["metric"].as_str().unwrap()).collect();
        // Unknown figures, NaN and other teams' databases are dropped.
        assert_eq!(
            metrics,
            ["cpu", "disk_used_gi", "disk_capacity_gi", "transactions"]
        );
        assert!(rows.iter().all(|r| r["database_id"] == "db_1"
            && r["team_id"] == "team_a"
            && r["member"] == "pg-1"));
    }
}
